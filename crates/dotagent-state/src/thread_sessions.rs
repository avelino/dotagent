//! Which conversation a group message belongs to.
//!
//! In a direct chat every message trivially belongs to the same
//! conversation. A group does not have that property: two questions asked a
//! minute apart are usually about different things, and one shared session
//! means the second answer is informed by the first subject — the mixing this
//! table exists to prevent.
//!
//! The binding is the Telegram reply chain. A message that replies to a
//! known message inherits its conversation; anything else starts one, and
//! both the inbound message and the bot's answer to it are recorded here so
//! replying to either keeps the thread.
//!
//! Shape (mirrors [`crate::sent_messages`] on purpose — same access pattern,
//! same bounded-lookup philosophy):
//!
//! ```jsonc
//! {
//!   "entries": {
//!     "10:-1001:4821": {              // chat id length, chat id, message id
//!     "chat_id": "-1001",
//!     "session": "-1001-r4821",
//!     "at": 1785925367
//!   }
//! }
//! }
//! ```
//!
//! Bounded on purpose: this is what makes "reply to something from last
//! week" resolvable, not a history of every group the bot is in.

use std::collections::BTreeMap;
use std::fs::OpenOptions;
use std::path::PathBuf;

use fs2::FileExt;
use serde::{Deserialize, Serialize};

use crate::StateError;

/// How many messages stay bound to their conversation. Oldest dropped first.
const MAX_ENTRIES: usize = 1000;

/// One message's conversation binding.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ThreadSession {
    /// Telegram chat that owns the message — Telegram scopes `message_id`
    /// to a chat, so a bare id can never be resolved safely.
    pub chat_id: String,
    /// Conversation key derived at ingress (the gateway's session id).
    pub session: String,
    /// Unix seconds, used only to decide what to drop when full.
    pub at: i64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ThreadSessions {
    #[serde(default)]
    pub entries: BTreeMap<String, ThreadSession>,
}

impl ThreadSessions {
    pub fn get_for_chat(&self, chat_id: &str, message_id: i64) -> Option<&str> {
        self.entries
            .get(&scoped_key(chat_id, message_id))
            .filter(|entry| entry.chat_id == chat_id)
            .map(|entry| entry.session.as_str())
    }

    /// Bind one message to its conversation, evicting the oldest when full.
    pub fn insert_for_chat(&mut self, chat_id: &str, message_id: i64, session: &str, at: i64) {
        self.entries.insert(
            scoped_key(chat_id, message_id),
            ThreadSession {
                chat_id: chat_id.to_string(),
                session: session.to_string(),
                at,
            },
        );
        self.evict_oldest();
    }

    fn evict_oldest(&mut self) {
        while self.entries.len() > MAX_ENTRIES {
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

/// Reader/writer for the conversation binding table.
#[derive(Debug, Clone)]
pub struct ThreadSessionStore {
    path: PathBuf,
}

impl ThreadSessionStore {
    pub fn new(path: PathBuf) -> Self {
        Self { path }
    }

    pub fn from_home() -> Self {
        Self::new(crate::paths::telegram_threads_file())
    }

    /// An unreadable or corrupt table costs thread continuity, never
    /// delivery: an unresolvable reply starts a new conversation, which is
    /// the pre-table behaviour for every message.
    pub fn load(&self) -> ThreadSessions {
        if !self.path.exists() {
            return ThreadSessions::default();
        }
        std::fs::File::open(&self.path)
            .ok()
            .and_then(|f| serde_json::from_reader(f).ok())
            .unwrap_or_default()
    }

    /// Bind a Telegram message to its conversation inside its chat.
    pub fn record_for_chat(
        &self,
        chat_id: &str,
        message_id: i64,
        session: &str,
        at: i64,
    ) -> Result<(), StateError> {
        self.with_locked_table(|table| table.insert_for_chat(chat_id, message_id, session, at))
    }

    /// Resolve a Telegram message to its conversation, if it has one.
    pub fn resolve_for_chat(&self, chat_id: &str, message_id: i64) -> Option<String> {
        self.load()
            .get_for_chat(chat_id, message_id)
            .map(String::from)
    }

    fn with_locked_table<F>(&self, update: F) -> Result<(), StateError>
    where
        F: FnOnce(&mut ThreadSessions),
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

    fn write(&self, table: &ThreadSessions) -> Result<(), StateError> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let tmp = self.path.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_vec_pretty(table)?)?;
        std::fs::rename(&tmp, &self.path)?;
        Ok(())
    }
}

/// Length-prefix the chat id so the separator cannot make two pairs collide.
fn scoped_key(chat_id: &str, message_id: i64) -> String {
    format!("{}:{chat_id}:{message_id}", chat_id.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn records_and_resolves_within_a_chat() {
        let dir = tempfile::tempdir().unwrap();
        let store = ThreadSessionStore::new(dir.path().join("threads.json"));
        store
            .record_for_chat("-1001", 42, "-1001-r42", 100)
            .unwrap();

        assert_eq!(
            store.resolve_for_chat("-1001", 42).as_deref(),
            Some("-1001-r42")
        );
    }

    #[test]
    fn the_same_message_id_in_another_chat_resolves_to_nothing() {
        // Telegram scopes message ids to a chat; resolving across chats would
        // splice two groups' conversations together.
        let dir = tempfile::tempdir().unwrap();
        let store = ThreadSessionStore::new(dir.path().join("threads.json"));
        store
            .record_for_chat("-1001", 42, "-1001-r42", 100)
            .unwrap();

        assert!(store.resolve_for_chat("-1002", 42).is_none());
    }

    #[test]
    fn an_unbound_message_resolves_to_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let store = ThreadSessionStore::new(dir.path().join("threads.json"));
        assert!(store.resolve_for_chat("-1001", 7).is_none());
    }

    #[test]
    fn a_missing_or_corrupt_file_costs_continuity_not_delivery() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("threads.json");
        std::fs::write(&path, "{not json").unwrap();
        let store = ThreadSessionStore::new(path);
        assert!(store.resolve_for_chat("-1001", 7).is_none());
        // And it recovers: the next write replaces the garbage.
        store.record_for_chat("-1001", 8, "-1001-r8", 1).unwrap();
        assert!(store.resolve_for_chat("-1001", 8).is_some());
    }

    #[test]
    fn the_table_stays_bounded_and_drops_the_oldest() {
        let mut table = ThreadSessions::default();
        for i in 0..(MAX_ENTRIES as i64 + 10) {
            table.insert_for_chat("-1001", i, "-1001-r9", i);
        }
        assert_eq!(table.entries.len(), MAX_ENTRIES);
        assert!(table.get_for_chat("-1001", 0).is_none());
        assert!(table
            .get_for_chat("-1001", MAX_ENTRIES as i64 + 9)
            .is_some());
    }

    #[test]
    fn re_binding_a_message_overwrites_rather_than_duplicates() {
        let mut table = ThreadSessions::default();
        table.insert_for_chat("-1001", 42, "-1001-r42", 10);
        table.insert_for_chat("-1001", 42, "-1001-r99", 20);
        assert_eq!(table.entries.len(), 1);
        assert_eq!(table.get_for_chat("-1001", 42), Some("-1001-r99"));
    }

    #[test]
    fn concurrent_records_from_ingress_and_sink_never_lose_each_other() {
        // The ingress binds inbound messages while gateway workers bind the
        // bot's replies — two writers on one table.
        const THREADS: usize = 4;
        const PER_THREAD: usize = 32;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("threads.json");
        let start = std::sync::Arc::new(std::sync::Barrier::new(THREADS));

        std::thread::scope(|scope| {
            for thread_id in 0..THREADS {
                let store = ThreadSessionStore::new(path.clone());
                let start = std::sync::Arc::clone(&start);
                scope.spawn(move || {
                    start.wait();
                    for index in 0..PER_THREAD {
                        let message_id = (thread_id * PER_THREAD + index) as i64;
                        store
                            .record_for_chat("-1001", message_id, "-1001-r1", message_id)
                            .unwrap();
                    }
                });
            }
        });

        let table = ThreadSessionStore::new(path.clone()).load();
        assert_eq!(table.entries.len(), THREADS * PER_THREAD);
        assert!(path.with_extension("lock").exists());
        assert!(!path.with_extension("json.tmp").exists());
    }
}
