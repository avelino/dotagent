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
//!     "5:-1001:4821": {                // chat id length, chat id, message id
//!       "chat_id": "-1001",
//!       "agent": "inbox-triage",
//!       "schedule": "every-90min",
//!       "event": "preflight",
//!       "at": 1785925367
//!     },
//!     "4821": {                        // legacy, unscoped message_id
//!       "agent": "old-agent",
//!       "schedule": "daily",
//!       "event": "given_up",
//!       "at": 1785925000
//!     }
//!   }
//! }
//! ```
//!
//! Bounded on purpose. This is a lookup table for recent notifications, not a
//! history: a chat where every failure for a year is answerable is a file that
//! grows forever to serve a question nobody asks about last March.

use std::collections::BTreeMap;
use std::fs::OpenOptions;
use std::path::PathBuf;

use fs2::FileExt;
use serde::{Deserialize, Serialize};

use crate::StateError;

/// How many notifications stay resolvable. Oldest are dropped first.
const MAX_ENTRIES: usize = 500;

/// What one outbound notification was about.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SentMessage {
    /// Telegram chat that owns the message. `None` marks a legacy record that
    /// is only available through the unscoped compatibility methods.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chat_id: Option<String>,
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

    /// Resolve a message only inside its Telegram chat.
    ///
    /// Legacy records without `chat_id` are deliberately excluded. A global
    /// message id cannot be correlated safely because Telegram scopes ids to a
    /// chat, so falling back to such a record could expose another chat's run.
    pub fn get_for_chat(&self, chat_id: &str, message_id: i64) -> Option<&SentMessage> {
        self.entries
            .get(&scoped_key(chat_id, message_id))
            .filter(|entry| entry.chat_id.as_deref() == Some(chat_id))
    }

    /// Record one notification, evicting the oldest when full.
    pub fn insert(&mut self, message_id: i64, entry: SentMessage) {
        self.entries.insert(message_id.to_string(), entry);
        self.evict_oldest();
    }

    /// Record one notification for a specific Telegram chat.
    pub fn insert_for_chat(&mut self, chat_id: &str, message_id: i64, mut entry: SentMessage) {
        entry.chat_id = Some(chat_id.to_string());
        self.entries.insert(scoped_key(chat_id, message_id), entry);
        self.evict_oldest();
    }

    fn evict_oldest(&mut self) {
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
        self.with_locked_table(|table| table.insert(message_id, entry))
    }

    /// Record what a Telegram message was about inside its chat.
    pub fn record_for_chat(
        &self,
        chat_id: &str,
        message_id: i64,
        entry: SentMessage,
    ) -> Result<(), StateError> {
        self.with_locked_table(|table| table.insert_for_chat(chat_id, message_id, entry))
    }

    fn with_locked_table<F>(&self, update: F) -> Result<(), StateError>
    where
        F: FnOnce(&mut SentMessages),
    {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let lock = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .read(true)
            .open(self.path.with_extension("lock"))?;
        lock.lock_exclusive()?;

        let mut table = self.load();
        update(&mut table);
        let written = self.write(&table);
        drop(lock);
        written
    }

    fn write(&self, table: &SentMessages) -> Result<(), StateError> {
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

    /// Resolve a Telegram message only inside its chat.
    pub fn resolve_for_chat(&self, chat_id: &str, message_id: i64) -> Option<SentMessage> {
        self.load().get_for_chat(chat_id, message_id).cloned()
    }
}

/// Length-prefix the chat id so the separator cannot make two pairs collide.
fn scoped_key(chat_id: &str, message_id: i64) -> String {
    format!("{}:{chat_id}:{message_id}", chat_id.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(agent: &str, at: i64) -> SentMessage {
        SentMessage {
            chat_id: None,
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

    #[test]
    fn scoped_message_ids_are_isolated_between_chats() {
        let mut table = SentMessages::default();
        table.insert_for_chat("chat-a", 42, entry("first", 10));
        table.insert_for_chat("chat-b", 42, entry("second", 20));

        assert_eq!(table.entries.len(), 2);
        assert_eq!(table.get_for_chat("chat-a", 42).unwrap().agent, "first");
        assert_eq!(table.get_for_chat("chat-b", 42).unwrap().agent, "second");
        assert!(table.get_for_chat("chat-c", 42).is_none());
        assert_eq!(
            table.get_for_chat("chat-a", 42).unwrap().chat_id.as_deref(),
            Some("chat-a")
        );
    }

    #[test]
    fn legacy_records_are_not_used_for_scoped_correlation() {
        let table: SentMessages = serde_json::from_str(
            r#"{"entries":{"42":{"agent":"legacy","schedule":"daily","event":"given_up","at":10}}}"#,
        )
        .unwrap();

        assert_eq!(table.get(42).unwrap().agent, "legacy");
        assert!(table.get_for_chat("chat-a", 42).is_none());
    }

    #[test]
    fn record_for_chat_persists_chat_id_and_scoped_key() {
        let dir = tempfile::tempdir().unwrap();
        let store = SentMessageStore::new(dir.path().join("sent.json"));

        store
            .record_for_chat("chat-a", 4821, entry("inbox-triage", 100))
            .unwrap();

        let raw = std::fs::read_to_string(dir.path().join("sent.json")).unwrap();
        assert!(raw.contains("6:chat-a:4821"));
        assert!(raw.contains("\"chat_id\": \"chat-a\""));
        assert_eq!(
            store
                .resolve_for_chat("chat-a", 4821)
                .unwrap()
                .chat_id
                .as_deref(),
            Some("chat-a")
        );
        assert!(store.resolve_for_chat("chat-b", 4821).is_none());
        assert!(store.resolve(4821).is_none());
    }

    #[test]
    fn concurrent_records_preserve_legacy_and_scoped_entries() {
        const THREADS: usize = 8;
        const ENTRIES_PER_THREAD: usize = 32;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sent.json");
        let start = std::sync::Arc::new(std::sync::Barrier::new(THREADS));

        std::thread::scope(|scope| {
            for thread_id in 0..THREADS {
                let path = path.clone();
                let start = std::sync::Arc::clone(&start);
                scope.spawn(move || {
                    let store = SentMessageStore::new(path);
                    let chat_id = format!("chat-{thread_id}");
                    start.wait();

                    for index in 0..ENTRIES_PER_THREAD {
                        let message_id = (thread_id * ENTRIES_PER_THREAD + index) as i64;
                        let at = message_id;
                        if index % 2 == 0 {
                            store
                                .record(message_id, entry(&format!("legacy-{thread_id}"), at))
                                .unwrap();
                        } else {
                            store
                                .record_for_chat(
                                    &chat_id,
                                    message_id,
                                    entry(&format!("scoped-{thread_id}"), at),
                                )
                                .unwrap();
                        }
                    }
                });
            }
        });

        let store = SentMessageStore::new(path.clone());
        let table = store.load();
        assert_eq!(table.entries.len(), THREADS * ENTRIES_PER_THREAD);
        for thread_id in 0..THREADS {
            let chat_id = format!("chat-{thread_id}");
            for index in 0..ENTRIES_PER_THREAD {
                let message_id = (thread_id * ENTRIES_PER_THREAD + index) as i64;
                if index % 2 == 0 {
                    assert!(table.get(message_id).is_some());
                } else {
                    assert!(table.get_for_chat(&chat_id, message_id).is_some());
                }
            }
        }
        assert!(path.with_extension("lock").exists());
        assert!(!path.with_extension("json.tmp").exists());
    }
}
