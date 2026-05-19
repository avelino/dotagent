//! Append-only, hash-chained audit log.
//!
//! Default path: `~/.local/share/dotagent/audit.log` (one JSON object per
//! line). Each entry's `prev_hash` is sha256 of the previous line's full
//! JSON. The first entry has `prev_hash = "GENESIS"`.
//!
//! Tamper detection: on startup the daemon recomputes the chain. A
//! mismatch emits `AuditEvent::AuditChainBroken` (which is itself an
//! audit entry, anchored to the broken position).

use std::fs::OpenOptions;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};

use chrono::Local;
use dotagent_core::audit::{AuditEntry, AuditEvent, Severity, GENESIS_HASH};
use fs2::FileExt;
use thiserror::Error;

pub type Result<T> = std::result::Result<T, AuditError>;

#[derive(Debug, Error)]
pub enum AuditError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("json: {0}")]
    Json(#[from] serde_json::Error),
    #[error("no home directory")]
    NoHome,
}

/// Filesystem-backed append-only audit log.
#[derive(Debug, Clone)]
pub struct AuditLog {
    path: PathBuf,
}

impl AuditLog {
    /// Default location: `$DOTAGENT_HOME/audit.log`.
    pub fn from_home() -> Result<Self> {
        Ok(Self::with_path(crate::paths::audit_log_file()))
    }

    pub fn with_path(path: PathBuf) -> Self {
        Self { path }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Append an event. Returns the resulting entry (with `ts` and chained
    /// `prev_hash` filled in).
    pub fn append(&self, event: AuditEvent) -> Result<AuditEntry> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let lock_path = self.path.with_extension("log.lock");
        let lock = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .read(true)
            .open(&lock_path)?;
        lock.lock_exclusive()?;

        let prev_hash = self.tail_hash_locked()?;
        let entry = AuditEntry {
            ts: Local::now().format("%Y-%m-%dT%H:%M:%S%z").to_string(),
            severity: event.default_severity(),
            event,
            prev_hash,
        };
        let line = serde_json::to_string(&entry)?;

        let mut f = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)?;
        f.write_all(line.as_bytes())?;
        f.write_all(b"\n")?;
        f.sync_all()?;

        // best-effort: drop lock file
        let _ = std::fs::remove_file(&lock_path);
        drop(lock);
        Ok(entry)
    }

    /// Walk every entry. Useful for `dotagent status --audit`.
    pub fn iter_entries(&self) -> Result<Vec<AuditEntry>> {
        if !self.path.exists() {
            return Ok(Vec::new());
        }
        let f = std::fs::File::open(&self.path)?;
        let reader = BufReader::new(f);
        let mut out = Vec::new();
        for line in reader.lines() {
            let line = line?;
            if line.trim().is_empty() {
                continue;
            }
            out.push(serde_json::from_str(&line)?);
        }
        Ok(out)
    }

    /// Verify the hash chain end-to-end. Returns `Ok(None)` if intact, or
    /// `Ok(Some((position, expected, actual)))` at the first mismatch.
    pub fn verify_chain(&self) -> Result<Option<ChainBreak>> {
        let entries = self.iter_entries()?;
        let mut expected_prev = GENESIS_HASH.to_string();
        for (i, entry) in entries.iter().enumerate() {
            if entry.prev_hash != expected_prev {
                return Ok(Some(ChainBreak {
                    position: i,
                    expected: expected_prev,
                    actual: entry.prev_hash.clone(),
                }));
            }
            expected_prev = hash_line(&serde_json::to_string(entry)?);
        }
        Ok(None)
    }

    /// Returns the hash of the last line, or "GENESIS" if the log is empty.
    fn tail_hash_locked(&self) -> Result<String> {
        if !self.path.exists() {
            return Ok(GENESIS_HASH.to_string());
        }
        // For large logs, reading the entire file is wasteful — but audit
        // logs grow slowly (handful of events per agent run) and we already
        // hold the lock. Optimize later if it becomes a bottleneck.
        let f = std::fs::File::open(&self.path)?;
        let reader = BufReader::new(f);
        let last = reader
            .lines()
            .map_while(std::result::Result::ok)
            .filter(|l| !l.trim().is_empty())
            .last();
        match last {
            None => Ok(GENESIS_HASH.to_string()),
            Some(line) => Ok(hash_line(&line)),
        }
    }
}

#[derive(Debug, Clone)]
pub struct ChainBreak {
    pub position: usize,
    pub expected: String,
    pub actual: String,
}

/// sha256 hex of a string.
pub fn hash_line(s: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(s.as_bytes());
    let digest = hasher.finalize();
    hex_encode(&digest)
}

fn hex_encode(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for &b in bytes {
        out.push(HEX[(b >> 4) as usize] as char);
        out.push(HEX[(b & 0x0f) as usize] as char);
    }
    out
}

/// Helper: severity → bool, "should this fire out-of-band notify?"
pub fn is_critical(sev: Severity) -> bool {
    matches!(sev, Severity::Critical)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn append_chains_hashes() {
        let dir = tempdir().unwrap();
        let log = AuditLog::with_path(dir.path().join("audit.log"));

        let e1 = log
            .append(AuditEvent::DaemonStarted {
                version: "0.0.1".into(),
                pid: 1,
            })
            .unwrap();
        assert_eq!(e1.prev_hash, GENESIS_HASH);

        let e2 = log
            .append(AuditEvent::TickStarted { agents_scanned: 3 })
            .unwrap();
        assert_ne!(e2.prev_hash, GENESIS_HASH);

        let entries = log.iter_entries().unwrap();
        assert_eq!(entries.len(), 2);
        assert!(log.verify_chain().unwrap().is_none());
    }

    #[test]
    fn verify_chain_detects_tamper() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("audit.log");
        let log = AuditLog::with_path(path.clone());

        log.append(AuditEvent::DaemonStarted {
            version: "0.0.1".into(),
            pid: 1,
        })
        .unwrap();
        log.append(AuditEvent::TickStarted { agents_scanned: 1 })
            .unwrap();

        // Tamper: rewrite the file with a broken chain
        let raw = std::fs::read_to_string(&path).unwrap();
        let mut lines: Vec<String> = raw.lines().map(String::from).collect();
        lines[1] = lines[1].replace("\"prev_hash\":\"", "\"prev_hash\":\"deadbeef");
        std::fs::write(&path, lines.join("\n") + "\n").unwrap();

        let result = log.verify_chain().unwrap();
        assert!(result.is_some());
        assert_eq!(result.unwrap().position, 1);
    }
}
