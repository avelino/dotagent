//! Conversation registry store: the IO edge for [`RegistryRecord`] files.
//!
//! One JSON file per conversation under `<state>/assistant/`, written
//! tmp-then-rename with 0600 permissions — same conventions as
//! `dotagent-state`. The daemon is the only writer (the gateway serializes
//! runs per conversation), so no cross-process lock is taken; a torn write
//! is impossible by construction and a lost update would cost one session
//! pointer, never data.
//!
//! File names are derived from `(source, session_id)`: the pair is
//! sanitized to filesystem-safe characters and, when sanitizing changed
//! anything, a short hash of the raw key is appended so two distinct
//! conversations can never collapse into the same file.

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use serde::Deserialize;
use serde::Serialize;
use serde_json::Value;
use thiserror::Error;

use crate::registry::RegistryRecord;

/// Failures of the registry store. None of them are fatal to a conversation
/// turn: the caller treats a load failure as "no session pointer" and a
/// save failure as a lost pointer — degraded, not broken.
#[derive(Debug, Error)]
pub enum StoreError {
    #[error("registry io: {0}")]
    Io(#[from] std::io::Error),
    #[error("registry file is not valid JSON: {0}")]
    Corrupt(#[from] serde_json::Error),
}

/// What gets persisted: the record's pointers plus a write timestamp.
///
/// The wrapper keeps `updated_at` out of the pure [`RegistryRecord`] —
/// transitions never read it, only the store stamps it.
#[derive(Debug, Serialize, Deserialize)]
struct StoredRecord {
    #[serde(flatten)]
    record: RegistryRecord,
    updated_at: DateTime<Utc>,
}

/// File-backed conversation registry rooted at a directory the daemon owns.
#[derive(Debug, Clone)]
pub struct RegistryStore {
    dir: PathBuf,
}

impl RegistryStore {
    /// Create a store view over `<root>` (created lazily on first save).
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { dir: root.into() }
    }

    /// Load the record for a conversation, if one was ever saved.
    ///
    /// A missing file is `Ok(None)` — the common case for a first message.
    /// A corrupt file is an `Err`: silent ignore would hide a real defect.
    pub fn load(
        &self,
        source: &str,
        session_id: &str,
    ) -> Result<Option<RegistryRecord>, StoreError> {
        let path = self.path_for(source, session_id);
        if !path.exists() {
            return Ok(None);
        }
        let bytes = fs::read(&path)?;
        let stored: StoredRecord = serde_json::from_slice(&bytes)?;
        Ok(Some(stored.record))
    }

    /// Persist a record atomically: write tmp with 0600, fsync, rename.
    pub fn save(&self, record: &RegistryRecord, now: DateTime<Utc>) -> Result<(), StoreError> {
        fs::create_dir_all(&self.dir)?;
        let path = self.path_for(&record.source, &record.session_id);
        let tmp = path.with_extension("json.tmp");

        let stored = StoredRecord {
            record: record.clone(),
            updated_at: now,
        };
        let bytes = serde_json::to_vec_pretty(&stored)?;

        {
            let mut f = OpenOptions::new()
                .create(true)
                .truncate(true)
                .write(true)
                .open(&tmp)?;
            f.write_all(&bytes)?;
            f.sync_all()?;
        }
        // 0600 before the rename so the file never exists world-readable,
        // even for the instant between rename and chmod.
        fs::set_permissions(&tmp, fs::Permissions::from_mode(0o600))?;
        fs::rename(&tmp, &path)?;
        Ok(())
    }

    /// Filesystem-safe, collision-free name for a conversation key.
    fn path_for(&self, source: &str, session_id: &str) -> PathBuf {
        let raw = format!("{source}-{session_id}");
        let sanitized: String = sanitize(&raw);
        let name = if sanitized == raw {
            sanitized
        } else {
            // Sanitizing lost information; append a digest of the raw key
            // so "chat/a" and "chat:a" stay distinct files.
            let digest = short_digest(&raw);
            format!("{sanitized}-{digest}")
        };
        self.dir.join(format!("{name}.json"))
    }
}

/// Keep alphanumerics, dash, underscore and dot; drop the rest.
fn sanitize(raw: &str) -> String {
    raw.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// First 8 hex chars of a sha256 — enough to separate keys, not a security
/// boundary (the state dir is already 0700-and-below user territory).
fn short_digest(raw: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(raw.as_bytes());
    let digest = hasher.finalize();
    digest.iter().take(4).map(|b| format!("{b:02x}")).collect()
}

/// The directory this store writes to (for tests and daemon wiring).
pub fn store_dir(root: &Path) -> PathBuf {
    root.join("assistant")
}

/// Write the assembled `mcp.json` to its content-addressed path, reusing
/// the existing file when the bytes are identical.
///
/// The file name is `toolkit-<hash>.json` — two declarations that hash the
/// same share one file, so adding a second assistant agent with the same
/// toolkit costs nothing and a changed toolkit never overwrites the old
/// config a resumed session might still point at.
pub fn ensure_toolkit_file(dir: &Path, config: &Value) -> Result<(PathBuf, String), StoreError> {
    use crate::toolkit::toolkit_hash;

    let hash = toolkit_hash(config);
    let path = dir.join(format!("toolkit-{hash}.json"));
    if path.exists() {
        return Ok((path, hash));
    }
    fs::create_dir_all(dir)?;
    let tmp = dir.join(format!("toolkit-{hash}.json.tmp"));
    let bytes = serde_json::to_vec_pretty(config)?;
    {
        let mut f = OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .open(&tmp)?;
        f.write_all(&bytes)?;
        f.sync_all()?;
    }
    fs::set_permissions(&tmp, fs::Permissions::from_mode(0o600))?;
    fs::rename(&tmp, &path)?;
    Ok((path, hash))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::registry::DEFAULT_TRANSCRIPT_BYTES_MAX;
    use tempfile::tempdir;

    fn saved_record() -> RegistryRecord {
        let mut r = RegistryRecord::new("telegram", "12345");
        r.note_toolkit_hash("hash-a");
        r.apply_session_frame("s-1", 1_000, DEFAULT_TRANSCRIPT_BYTES_MAX);
        r
    }

    #[test]
    fn round_trips_a_record() {
        let dir = tempdir().unwrap();
        let store = RegistryStore::new(store_dir(dir.path()));
        let record = saved_record();
        store.save(&record, Utc::now()).expect("save must succeed");
        let loaded = store.load("telegram", "12345").expect("load must succeed");
        assert_eq!(loaded, Some(record));
    }

    #[test]
    fn missing_file_loads_none() {
        let dir = tempdir().unwrap();
        let store = RegistryStore::new(store_dir(dir.path()));
        assert_eq!(store.load("telegram", "never-seen").unwrap(), None);
    }

    #[test]
    fn file_is_created_with_0600() {
        let dir = tempdir().unwrap();
        let store = RegistryStore::new(store_dir(dir.path()));
        store.save(&saved_record(), Utc::now()).unwrap();
        let path = store.path_for("telegram", "12345");
        let mode = fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    #[test]
    fn save_leaves_no_tmp_file_behind() {
        let dir = tempdir().unwrap();
        let store = RegistryStore::new(store_dir(dir.path()));
        store.save(&saved_record(), Utc::now()).unwrap();
        let tmp = store
            .path_for("telegram", "12345")
            .with_extension("json.tmp");
        assert!(!tmp.exists());
    }

    #[test]
    fn corrupted_file_is_an_error_not_a_lie() {
        let dir = tempdir().unwrap();
        let store = RegistryStore::new(store_dir(dir.path()));
        let path = store.path_for("telegram", "12345");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, b"{ not json").unwrap();
        assert!(store.load("telegram", "12345").is_err());
    }

    #[test]
    fn persisted_json_has_updated_at_and_no_content_fields() {
        let dir = tempdir().unwrap();
        let store = RegistryStore::new(store_dir(dir.path()));
        store.save(&saved_record(), Utc::now()).unwrap();
        let bytes = fs::read(store.path_for("telegram", "12345")).unwrap();
        let text = String::from_utf8(bytes).unwrap();
        assert!(text.contains("\"updated_at\""));
        assert!(!text.contains("messages"));
        assert!(!text.contains("transcript_text"));
    }

    #[test]
    fn keys_needing_sanitization_stay_distinct() {
        let dir = tempdir().unwrap();
        let store = RegistryStore::new(store_dir(dir.path()));
        let a = RegistryRecord::new("local", "chat/a");
        let b = RegistryRecord::new("local", "chat:a");
        store.save(&a, Utc::now()).unwrap();
        store.save(&b, Utc::now()).unwrap();
        assert_eq!(store.load("local", "chat/a").unwrap(), Some(a));
        assert_eq!(store.load("local", "chat:a").unwrap(), Some(b));
        // Two files, not one clobbering the other.
        let count = fs::read_dir(store_dir(dir.path())).unwrap().count();
        assert_eq!(count, 2);
    }

    #[test]
    fn overwrite_replaces_the_same_file() {
        let dir = tempdir().unwrap();
        let store = RegistryStore::new(store_dir(dir.path()));
        store.save(&saved_record(), Utc::now()).unwrap();

        let mut evolved = saved_record();
        evolved.apply_session_frame("s-2", 2_000, DEFAULT_TRANSCRIPT_BYTES_MAX);
        store.save(&evolved, Utc::now()).unwrap();

        assert_eq!(store.load("telegram", "12345").unwrap(), Some(evolved));
        let count = fs::read_dir(store_dir(dir.path())).unwrap().count();
        assert_eq!(count, 1, "second save must replace, not accumulate");
    }

    #[test]
    fn toolkit_file_is_content_addressed_and_reused() {
        let dir = tempdir().unwrap();
        let config = serde_json::json!({"mcp": {"type": "http", "url": "http://x/mcp"}});

        let (path_a, hash_a) = ensure_toolkit_file(dir.path(), &config).unwrap();
        let (path_b, hash_b) = ensure_toolkit_file(dir.path(), &config).unwrap();

        assert_eq!(path_a, path_b, "same bytes → same path");
        assert_eq!(hash_a, hash_b);
        assert!(path_a
            .file_name()
            .unwrap()
            .to_string_lossy()
            .contains(&hash_a));
        // The second call must not leave a temp file or a duplicate.
        let count = fs::read_dir(dir.path()).unwrap().count();
        assert_eq!(count, 1);
    }

    #[test]
    fn toolkit_file_is_0600() {
        let dir = tempdir().unwrap();
        let config = serde_json::json!({"mcp": {"type": "http", "url": "http://x/mcp"}});
        let (path, _) = ensure_toolkit_file(dir.path(), &config).unwrap();
        let mode = fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    #[test]
    fn changed_toolkit_gets_a_new_file_both_remain() {
        let dir = tempdir().unwrap();
        let a = serde_json::json!({"mcp": {"type": "http", "url": "http://a/mcp"}});
        let b = serde_json::json!({"mcp": {"type": "http", "url": "http://b/mcp"}});

        let (path_a, hash_a) = ensure_toolkit_file(dir.path(), &a).unwrap();
        let (path_b, hash_b) = ensure_toolkit_file(dir.path(), &b).unwrap();

        assert_ne!(path_a, path_b);
        assert_ne!(hash_a, hash_b);
        // Old configs survive: a session resumed under the old toolkit may
        // still reference its file.
        assert!(path_a.exists());
        let count = fs::read_dir(dir.path()).unwrap().count();
        assert_eq!(count, 2);
    }
}
