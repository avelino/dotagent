//! Append-only, hash-chained audit log.
//!
//! Default path: `~/.local/share/dotagent/audit.log` (one JSON object per
//! line). Each entry's `prev_hash` is sha256 of the previous line's full
//! JSON. The first entry has `prev_hash = "GENESIS"`.
//!
//! Tamper detection: on startup the daemon recomputes the chain. A
//! mismatch emits `AuditEvent::AuditChainBroken` (which is itself an
//! audit entry, anchored to the broken position).
//!
//! # Rotation
//!
//! Past [`DEFAULT_MAX_BYTES`] the file is renamed to
//! `audit.log.<YYYYMMDDTHHMMSS>` and a fresh `audit.log` opens with a **seam**
//! ([`AuditEvent::AuditLogRotated`]) as its first line. The seam's `prev_hash`
//! is the segment's tail hash, so the chain crosses the rename intact, and it
//! names the segment it came from — which is what makes a deleted old segment
//! ("intact since <ts>") distinguishable from a log somebody beheaded
//! ("unexplained truncation"). See [`ChainStatus`].
//!
//! Segments are never deleted automatically. This is the only forensic
//! artifact dotagent keeps; pruning it is the operator's call, not a sweeper's.

use std::collections::HashSet;
use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom, Write};
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

/// Rotate once the live file passes this. Big enough that a normal install
/// (~110 entries/day, ~1KB each) never reaches it, small enough that a
/// runaway agent cannot leave a single file nobody can open.
pub const DEFAULT_MAX_BYTES: u64 = 32 * 1024 * 1024;

/// Filesystem-backed append-only audit log.
#[derive(Debug, Clone)]
pub struct AuditLog {
    path: PathBuf,
    max_bytes: u64,
}

impl AuditLog {
    /// Default location: `$DOTAGENT_HOME/audit.log`.
    pub fn from_home() -> Result<Self> {
        Ok(Self::with_path(crate::paths::audit_log_file()))
    }

    pub fn with_path(path: PathBuf) -> Self {
        Self {
            path,
            max_bytes: DEFAULT_MAX_BYTES,
        }
    }

    /// Override the rotation threshold. Mostly for tests — 32MB of real
    /// entries is not something a test wants to write.
    pub fn with_max_bytes(mut self, max_bytes: u64) -> Self {
        self.max_bytes = max_bytes;
        self
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

        let mut tail = self.tail_locked()?;
        // Rotation happens under the same lock as the append that tripped it,
        // so no writer can observe the file between the rename and the seam.
        if tail.len >= self.max_bytes && tail.last_line.is_some() {
            tail = self.rotate_locked(&tail)?;
        }
        let prev_hash = match &tail.last_line {
            Some(line) => hash_line(line),
            None => GENESIS_HASH.to_string(),
        };
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
        // A writer that died between the JSON and its newline leaves a line
        // with no terminator. Appending straight onto it would glue two
        // entries into one unparseable line — one corrupt record is bad, two
        // is worse.
        if !tail.ends_with_newline {
            f.write_all(b"\n")?;
        }
        f.write_all(line.as_bytes())?;
        f.write_all(b"\n")?;
        f.sync_all()?;

        // The lock file stays on disk. Unlinking it while still holding the
        // lock is what `write_json` in `lib.rs` warns about: the next writer
        // opens a *fresh inode* and takes its own "exclusive" lock, so two
        // appenders read the same tail and write two entries with the same
        // `prev_hash` — a chain fork that looks exactly like tampering.
        drop(lock);
        Ok(entry)
    }

    /// Walk every entry in the **current segment**. Useful for
    /// `dotagent status --audit`.
    pub fn iter_entries(&self) -> Result<Vec<AuditEntry>> {
        read_entries(&self.path)
    }

    /// Rotated segments still on disk, oldest first.
    ///
    /// Ordering is by file name, which sorts chronologically by construction.
    /// It is a convenience for listing, never the basis of verification —
    /// [`Self::verify_chain_status`] follows the seams, which are hashes.
    pub fn segments(&self) -> Result<Vec<PathBuf>> {
        let Some(dir) = self.path.parent() else {
            return Ok(Vec::new());
        };
        let Some(stem) = self.path.file_name().and_then(|s| s.to_str()) else {
            return Ok(Vec::new());
        };
        if !dir.exists() {
            return Ok(Vec::new());
        }
        let mut out = Vec::new();
        for entry in std::fs::read_dir(dir)? {
            let entry = entry?;
            let name = entry.file_name();
            let Some(name) = name.to_str() else { continue };
            if is_segment_name(stem, name) {
                out.push(entry.path());
            }
        }
        out.sort();
        Ok(out)
    }

    /// Verify the hash chain. Returns `Ok(None)` when the log is trustworthy,
    /// or the first mismatch.
    ///
    /// Kept for callers that only want yes/no — a legitimately rotated log
    /// reads as intact here. [`Self::verify_chain_status`] is the one that
    /// says *how far back* the guarantee reaches.
    pub fn verify_chain(&self) -> Result<Option<ChainBreak>> {
        match self.verify_chain_status(VerifyScope::CurrentSegment)? {
            ChainStatus::IntactFromGenesis | ChainStatus::IntactSinceRotation { .. } => Ok(None),
            ChainStatus::Broken(brk) => Ok(Some(brk)),
            // A head that nothing accounts for is reported at position 0
            // against GENESIS, which is what this call meant before rotation
            // existed and what the daemon still renders.
            ChainStatus::UnexplainedTruncation { orphan_prev_hash } => Ok(Some(ChainBreak {
                position: 0,
                expected: GENESIS_HASH.to_string(),
                actual: orphan_prev_hash,
                segment: None,
            })),
        }
    }

    /// Verify the chain and report how far back the verification actually got.
    ///
    /// `CurrentSegment` checks the live file only — cheap, and the live file is
    /// the only one that changes, which is why the daemon uses it at boot.
    /// `Full` walks the seams backwards through every segment still present.
    pub fn verify_chain_status(&self, scope: VerifyScope) -> Result<ChainStatus> {
        let mut file = self.path.clone();
        let mut entries = read_entries(&file)?;
        let mut seen: HashSet<PathBuf> = HashSet::new();

        loop {
            let label = (file != self.path).then(|| {
                file.file_name()
                    .and_then(|s| s.to_str())
                    .unwrap_or_default()
            });
            if let Some(brk) = verify_links(&entries, label)? {
                return Ok(ChainStatus::Broken(brk));
            }
            let Some(head) = entries.first() else {
                // An empty live file is a log that has never been written. An
                // empty *segment* cannot explain the seam that points at it.
                return Ok(if file == self.path {
                    ChainStatus::IntactFromGenesis
                } else {
                    ChainStatus::Broken(ChainBreak {
                        position: 0,
                        expected: "non-empty segment".into(),
                        actual: "empty file".into(),
                        segment: label.map(String::from),
                    })
                });
            };

            let seam = match classify_head(head, &self.path) {
                Head::Genesis => return Ok(ChainStatus::IntactFromGenesis),
                Head::Orphan(h) => {
                    return Ok(ChainStatus::UnexplainedTruncation {
                        orphan_prev_hash: h,
                    })
                }
                Head::Seam(seam) => seam,
            };

            let segment_path = self.path.with_file_name(&seam.segment);
            let segment_present = segment_path.exists();
            let stop = matches!(scope, VerifyScope::CurrentSegment)
                || !segment_present
                // A seam pointing at a segment already walked is a cycle
                // somebody forged; refuse to follow it rather than spin.
                || !seen.insert(segment_path.clone());
            if stop {
                return Ok(ChainStatus::IntactSinceRotation {
                    since_ts: head.ts.clone(),
                    segment: seam.segment,
                    entries_before: seam.entries,
                    segment_present,
                });
            }

            // The seam claims the segment ends on `tail_hash`. That claim is
            // covered by the chain we just verified, so checking it against
            // the file is what links the two segments.
            let actual_tail = match read_tail(&segment_path)?.last_line {
                Some(line) => hash_line(&line),
                None => GENESIS_HASH.to_string(),
            };
            if actual_tail != seam.tail_hash {
                return Ok(ChainStatus::Broken(ChainBreak {
                    position: 0,
                    expected: seam.tail_hash,
                    actual: actual_tail,
                    segment: Some(seam.segment),
                }));
            }

            entries = read_entries(&segment_path)?;
            file = segment_path;
        }
    }

    /// Rename the live file to a segment and open a fresh one whose first line
    /// is the seam. Caller holds the append lock.
    fn rotate_locked(&self, tail: &Tail) -> Result<Tail> {
        let last_line = tail
            .last_line
            .as_ref()
            .expect("caller checked the log is non-empty");
        let tail_hash = hash_line(last_line);
        let (entries, first_ts) = summarize_segment(&self.path)?;

        let segment = self.next_segment_name()?;
        std::fs::rename(&self.path, self.path.with_file_name(&segment))?;

        let seam = AuditEntry {
            ts: Local::now().format("%Y-%m-%dT%H:%M:%S%z").to_string(),
            severity: Severity::Notice,
            event: AuditEvent::AuditLogRotated {
                rotated_to: segment,
                entries,
                first_ts,
                tail_hash: tail_hash.clone(),
            },
            prev_hash: tail_hash,
        };
        let line = serde_json::to_string(&seam)?;
        let mut f = OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .open(&self.path)?;
        f.write_all(line.as_bytes())?;
        f.write_all(b"\n")?;
        f.sync_all()?;

        Ok(Tail {
            len: line.len() as u64 + 1,
            last_line: Some(line),
            ends_with_newline: true,
            bytes_read: 0,
        })
    }

    /// `audit.log.<YYYYMMDDTHHMMSS>`, with `-N` appended if a rotation already
    /// happened this second.
    fn next_segment_name(&self) -> Result<String> {
        let stem = self
            .path
            .file_name()
            .and_then(|s| s.to_str())
            .ok_or_else(|| {
                AuditError::Io(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "audit log path has no file name",
                ))
            })?;
        let stamp = Local::now().format("%Y%m%dT%H%M%S").to_string();
        for n in 0..1000 {
            let name = if n == 0 {
                format!("{stem}.{stamp}")
            } else {
                format!("{stem}.{stamp}-{n}")
            };
            if !self.path.with_file_name(&name).exists() {
                return Ok(name);
            }
        }
        Err(AuditError::Io(std::io::Error::new(
            std::io::ErrorKind::AlreadyExists,
            "cannot find a free audit log segment name",
        )))
    }

    /// Read the end of the log. Caller must hold the append lock.
    fn tail_locked(&self) -> Result<Tail> {
        if !self.path.exists() {
            return Ok(Tail::empty());
        }
        read_tail(&self.path)
    }
}

/// How much we pull off disk per backward step. One step covers the last line
/// of any realistic audit log; the loop only spins again for a line longer
/// than this, or for a run of blank lines at the end.
const TAIL_CHUNK: u64 = 8192;

/// The end of an audit log — as much of it as the chain actually needs.
struct Tail {
    /// Last line whose content is not just whitespace, decoded exactly the
    /// way `BufRead::lines()` would have produced it (trailing `\r` dropped).
    last_line: Option<String>,
    /// False when the file's final byte is not `\n`.
    ends_with_newline: bool,
    /// Total size of the file. Free here (we seek to the end anyway) and it is
    /// what the rotation threshold is measured against.
    len: u64,
    /// Bytes actually pulled off disk. Read only by tests, which assert it
    /// stays bounded no matter how large the log gets.
    #[cfg_attr(not(test), allow(dead_code))]
    bytes_read: u64,
}

impl Tail {
    fn empty() -> Self {
        Self {
            last_line: None,
            ends_with_newline: true,
            len: 0,
            bytes_read: 0,
        }
    }
}

/// Find the last non-blank line by seeking from the end, so the cost of an
/// append depends on the length of the last line rather than on the size of
/// the log. A production log reaches tens of MB in a few months; scanning it
/// from byte zero on every append made each event cost more than the last.
fn read_tail(path: &Path) -> Result<Tail> {
    let mut f = File::open(path)?;
    let len = f.seek(SeekFrom::End(0))?;
    if len == 0 {
        return Ok(Tail::empty());
    }

    // Bytes at offsets `[pos, len)` that have not been split into lines yet.
    let mut pending: Vec<u8> = Vec::new();
    let mut pos = len;
    let mut bytes_read = 0u64;

    // Prime with the final chunk first, so `ends_with_newline` sees the real
    // last byte rather than whatever the line loop leaves behind.
    read_back(&mut f, &mut pos, &mut pending, &mut bytes_read)?;
    let ends_with_newline = pending.last() == Some(&b'\n');

    loop {
        while let Some(nl) = pending.iter().rposition(|&b| b == b'\n') {
            let line = decode_line(&pending[nl + 1..])?;
            if !line.trim().is_empty() {
                return Ok(Tail {
                    last_line: Some(line),
                    ends_with_newline,
                    len,
                    bytes_read,
                });
            }
            pending.truncate(nl);
        }
        if pos == 0 {
            // No newline left to the left: `pending` is the file's first line.
            let line = decode_line(&pending)?;
            return Ok(Tail {
                last_line: (!line.trim().is_empty()).then_some(line),
                ends_with_newline,
                len,
                bytes_read,
            });
        }
        read_back(&mut f, &mut pos, &mut pending, &mut bytes_read)?;
    }
}

/// Prepend the chunk ending at `pos` to `pending`, moving `pos` left.
fn read_back(
    f: &mut File,
    pos: &mut u64,
    pending: &mut Vec<u8>,
    bytes_read: &mut u64,
) -> Result<()> {
    let want = TAIL_CHUNK.min(*pos);
    let start = *pos - want;
    f.seek(SeekFrom::Start(start))?;
    let mut buf = vec![0u8; want as usize];
    f.read_exact(&mut buf)?;
    buf.extend_from_slice(pending);
    *pending = buf;
    *pos = start;
    *bytes_read += want;
    Ok(())
}

/// Chunk boundaries never split a line — a line is only decoded once both of
/// its delimiters are in hand — so this never sees a truncated UTF-8 sequence.
/// Invalid bytes therefore mean a corrupt log, and chaining onto an older line
/// would silently fork the chain. Fail instead.
fn decode_line(bytes: &[u8]) -> Result<String> {
    let bytes = match bytes.last() {
        Some(b'\r') => &bytes[..bytes.len() - 1],
        _ => bytes,
    };
    std::str::from_utf8(bytes)
        .map(str::to_string)
        .map_err(|e| AuditError::Io(std::io::Error::new(std::io::ErrorKind::InvalidData, e)))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChainBreak {
    pub position: usize,
    pub expected: String,
    pub actual: String,
    /// Rotated segment the break was found in, or `None` for the live file.
    pub segment: Option<String>,
}

/// How far back [`AuditLog::verify_chain_status`] should walk.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VerifyScope {
    /// The live `audit.log` only. It is the only file that changes, so this is
    /// what the daemon runs at boot.
    CurrentSegment,
    /// Follow the seams backwards through every rotated segment still present.
    Full,
}

/// What verification could actually establish.
///
/// The distinction between the last three is the point of a rotated audit log:
/// a history the operator legitimately pruned must not read the same as one an
/// attacker beheaded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChainStatus {
    /// Verified back to `GENESIS`. Every entry ever written was checked.
    IntactFromGenesis,
    /// Every entry checked links, and the oldest one is a seam
    /// ([`AuditEvent::AuditLogRotated`]) naming where the rest went.
    /// Verification stops there — the segment is either gone or out of scope.
    IntactSinceRotation {
        /// `ts` of the oldest entry that was verified.
        since_ts: String,
        /// Segment the seam points at.
        segment: String,
        /// How many entries lived in it, as the seam recorded at rotation time.
        entries_before: usize,
        /// `false` means it is no longer on disk — retention, or somebody
        /// deleting evidence, and the chain cannot tell those apart. `true`
        /// means it is there and simply was not walked
        /// ([`VerifyScope::CurrentSegment`]); re-run with [`VerifyScope::Full`].
        segment_present: bool,
    },
    /// The oldest entry checked links to a hash nothing accounts for: no
    /// `GENESIS`, no seam. Somebody removed the head of the log.
    UnexplainedTruncation { orphan_prev_hash: String },
    /// A link mismatch inside the data that is present.
    Broken(ChainBreak),
}

/// A validated seam, as read off the head of a file.
struct Seam {
    segment: String,
    entries: usize,
    tail_hash: String,
}

enum Head {
    Genesis,
    Seam(Seam),
    Orphan(String),
}

/// Decide what the first entry of a file claims about what came before it.
///
/// A seam only counts when its `prev_hash` equals the `tail_hash` it declares —
/// otherwise it is an entry shaped like a seam, which is exactly what forging
/// one looks like. `rotated_to` is validated as a bare sibling file name for
/// the same reason: it comes off disk, so it is attacker-writable, and a
/// verifier that opens `../../something` because a log line said so is a bug.
fn classify_head(head: &AuditEntry, log_path: &Path) -> Head {
    if head.prev_hash == GENESIS_HASH {
        return Head::Genesis;
    }
    if let AuditEvent::AuditLogRotated {
        rotated_to,
        entries,
        tail_hash,
        ..
    } = &head.event
    {
        let stem = log_path.file_name().and_then(|s| s.to_str()).unwrap_or("");
        if &head.prev_hash == tail_hash && is_segment_name(stem, rotated_to) {
            return Head::Seam(Seam {
                segment: rotated_to.clone(),
                entries: *entries,
                tail_hash: tail_hash.clone(),
            });
        }
    }
    Head::Orphan(head.prev_hash.clone())
}

/// `audit.log.20260806T101500` or `…-1`. Deliberately strict: it rejects
/// `audit.log.lock`, anything with a path separator, and anything that is not
/// a sibling of the live file.
fn is_segment_name(stem: &str, name: &str) -> bool {
    if name.contains('/') || name.contains('\\') || name.contains("..") {
        return false;
    }
    let Some(suffix) = name.strip_prefix(stem).and_then(|s| s.strip_prefix('.')) else {
        return false;
    };
    let (stamp, dup) = match suffix.split_once('-') {
        Some((stamp, dup)) => (stamp, Some(dup)),
        None => (suffix, None),
    };
    if let Some(dup) = dup {
        if dup.is_empty() || !dup.bytes().all(|b| b.is_ascii_digit()) {
            return false;
        }
    }
    stamp.len() == 15
        && stamp.as_bytes()[8] == b'T'
        && stamp
            .bytes()
            .enumerate()
            .all(|(i, b)| i == 8 || b.is_ascii_digit())
}

/// Parse every entry of one file. Blank lines are skipped, the same way the
/// tail read skips them.
fn read_entries(path: &Path) -> Result<Vec<AuditEntry>> {
    if !path.exists() {
        return Ok(Vec::new());
    }
    let reader = BufReader::new(File::open(path)?);
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

/// Check that consecutive entries link. Says nothing about where the file
/// starts — that is [`classify_head`]'s job, and conflating the two is what
/// would make a rotated log indistinguishable from a truncated one.
fn verify_links(entries: &[AuditEntry], segment: Option<&str>) -> Result<Option<ChainBreak>> {
    let mut expected: Option<String> = None;
    for (i, entry) in entries.iter().enumerate() {
        if let Some(exp) = &expected {
            if &entry.prev_hash != exp {
                return Ok(Some(ChainBreak {
                    position: i,
                    expected: exp.clone(),
                    actual: entry.prev_hash.clone(),
                    segment: segment.map(String::from),
                }));
            }
        }
        expected = Some(hash_line(&serde_json::to_string(entry)?));
    }
    Ok(None)
}

/// Count entries and pick up the first `ts`, for the seam to record. Runs once
/// per rotation (every 32MB), so a full read is fine here.
fn summarize_segment(path: &Path) -> Result<(usize, String)> {
    let reader = BufReader::new(File::open(path)?);
    let mut count = 0usize;
    let mut first_ts: Option<String> = None;
    for line in reader.lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        count += 1;
        if first_ts.is_none() {
            if let Ok(entry) = serde_json::from_str::<AuditEntry>(&line) {
                first_ts = Some(entry.ts);
            }
        }
    }
    Ok((count, first_ts.unwrap_or_else(|| "unknown".into())))
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

    // --- tail read -------------------------------------------------------

    /// The pre-seek implementation, kept verbatim so the fast path can be
    /// proven to hash exactly what the slow one hashed.
    fn legacy_tail_hash(path: &Path) -> String {
        if !path.exists() {
            return GENESIS_HASH.to_string();
        }
        let f = File::open(path).unwrap();
        let reader = BufReader::new(f);
        let last = reader
            .lines()
            .map_while(std::result::Result::ok)
            .filter(|l| !l.trim().is_empty())
            .last();
        match last {
            None => GENESIS_HASH.to_string(),
            Some(line) => hash_line(&line),
        }
    }

    fn tail_of(bytes: &[u8]) -> Tail {
        let dir = tempdir().unwrap();
        let path = dir.path().join("audit.log");
        std::fs::write(&path, bytes).unwrap();
        read_tail(&path).unwrap()
    }

    #[test]
    fn tail_of_empty_file_has_no_line() {
        let t = tail_of(b"");
        assert!(t.last_line.is_none());
        assert_eq!(t.bytes_read, 0);
    }

    #[test]
    fn tail_of_missing_file_is_genesis() {
        let dir = tempdir().unwrap();
        let log = AuditLog::with_path(dir.path().join("nope.log"));
        assert!(log.tail_locked().unwrap().last_line.is_none());
    }

    #[test]
    fn tail_of_single_line_without_trailing_newline() {
        let t = tail_of(b"only");
        assert_eq!(t.last_line.as_deref(), Some("only"));
        assert!(!t.ends_with_newline);
    }

    #[test]
    fn tail_of_single_line_with_trailing_newline() {
        let t = tail_of(b"only\n");
        assert_eq!(t.last_line.as_deref(), Some("only"));
        assert!(t.ends_with_newline);
    }

    #[test]
    fn tail_picks_the_last_of_many_lines() {
        let t = tail_of(b"a\nb\nc\n");
        assert_eq!(t.last_line.as_deref(), Some("c"));
    }

    #[test]
    fn tail_skips_trailing_blank_lines() {
        let t = tail_of(b"a\nlast\n\n   \n\n");
        assert_eq!(t.last_line.as_deref(), Some("last"));
        assert!(t.ends_with_newline);
    }

    #[test]
    fn tail_of_only_newlines_has_no_line() {
        assert!(tail_of(b"\n\n\n").last_line.is_none());
        assert!(tail_of(b"\n").last_line.is_none());
        assert!(tail_of(b"   \n").last_line.is_none());
    }

    #[test]
    fn tail_strips_carriage_return_like_bufread_lines() {
        let t = tail_of(b"a\r\nlast\r\n");
        assert_eq!(t.last_line.as_deref(), Some("last"));
    }

    #[test]
    fn tail_spans_chunk_boundaries_for_a_very_long_line() {
        // Three chunks worth of payload on the final line — the loop has to
        // walk left past several reads before it finds the delimiter.
        let long = "x".repeat(TAIL_CHUNK as usize * 3);
        let raw = format!("head\n{long}\n");
        let t = tail_of(raw.as_bytes());
        assert_eq!(t.last_line.as_deref(), Some(long.as_str()));
    }

    #[test]
    fn tail_handles_utf8_across_a_chunk_boundary() {
        // Pad so a multibyte char lands where a naive reader would cut.
        for pad in 0..8usize {
            let line = format!("{}héllo·ção", "y".repeat(TAIL_CHUNK as usize + pad));
            let raw = format!("head\n{line}\n");
            let t = tail_of(raw.as_bytes());
            assert_eq!(t.last_line.as_deref(), Some(line.as_str()), "pad={pad}");
        }
    }

    #[test]
    fn tail_rejects_a_line_that_is_not_utf8() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("audit.log");
        std::fs::write(&path, b"ok\n\xff\xfe\n").unwrap();
        assert!(read_tail(&path).is_err());
    }

    #[test]
    fn tail_hash_matches_the_full_scan_implementation() {
        let dir = tempdir().unwrap();
        let shapes: &[&[u8]] = &[
            b"",
            b"\n",
            b"one",
            b"one\n",
            b"a\nb\nc",
            b"a\nb\nc\n",
            b"a\nb\n\n\n",
            b"a\r\nb\r\n",
            b"  leading and trailing  \n",
            b"a\n\nb\n \n",
        ];
        for (i, shape) in shapes.iter().enumerate() {
            let path = dir.path().join(format!("s{i}.log"));
            std::fs::write(&path, shape).unwrap();
            let fast = match read_tail(&path).unwrap().last_line {
                Some(l) => hash_line(&l),
                None => GENESIS_HASH.to_string(),
            };
            assert_eq!(
                fast,
                legacy_tail_hash(&path),
                "shape {i}: {:?}",
                String::from_utf8_lossy(shape)
            );
        }
    }

    #[test]
    fn tail_hash_matches_the_full_scan_on_a_real_log() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("audit.log");
        let log = AuditLog::with_path(path.clone());
        for i in 0..200 {
            log.append(AuditEvent::TickStarted { agents_scanned: i })
                .unwrap();
        }
        let fast = hash_line(&read_tail(&path).unwrap().last_line.unwrap());
        assert_eq!(fast, legacy_tail_hash(&path));
        assert!(log.verify_chain().unwrap().is_none());
    }

    #[test]
    fn tail_read_does_not_scale_with_file_size() {
        // The point of the whole change: a 16MB log must cost the same to
        // append to as an empty one. Asserted in bytes, not wall clock, so a
        // loaded machine can't turn this into a flake.
        let dir = tempdir().unwrap();
        let path = dir.path().join("audit.log");
        let mut big = String::with_capacity(16 * 1024 * 1024);
        let mut n = 0u64;
        while big.len() < 16 * 1024 * 1024 {
            big.push_str(&format!(
                "{{\"ts\":\"2026-08-06T10:00:00-0300\",\"n\":{n},\"pad\":\"{}\"}}\n",
                "p".repeat(180)
            ));
            n += 1;
        }
        std::fs::write(&path, &big).unwrap();

        let t = read_tail(&path).unwrap();
        assert!(
            t.bytes_read <= TAIL_CHUNK,
            "read {} bytes off a {}-byte log",
            t.bytes_read,
            big.len()
        );
        assert_eq!(t.last_line.as_deref(), big.lines().next_back());
    }

    // --- append ----------------------------------------------------------

    #[test]
    fn append_heals_a_missing_trailing_newline() {
        // Simulates a writer that died between the JSON and its newline.
        let dir = tempdir().unwrap();
        let path = dir.path().join("audit.log");
        let log = AuditLog::with_path(path.clone());
        log.append(AuditEvent::TickStarted { agents_scanned: 1 })
            .unwrap();
        let mut raw = std::fs::read_to_string(&path).unwrap();
        raw.pop(); // drop the newline
        std::fs::write(&path, &raw).unwrap();

        log.append(AuditEvent::TickStarted { agents_scanned: 2 })
            .unwrap();

        let entries = log.iter_entries().unwrap();
        assert_eq!(entries.len(), 2, "the two entries must stay two lines");
        assert!(log.verify_chain().unwrap().is_none());
    }

    #[test]
    fn concurrent_appends_keep_the_chain_intact() {
        // Two appenders reading the same tail would write the same
        // `prev_hash` — a fork indistinguishable from tampering. The lock
        // file has to survive the append for flock to actually exclude.
        let dir = tempdir().unwrap();
        let path = dir.path().join("audit.log");

        std::thread::scope(|s| {
            for t in 0..4u32 {
                let path = path.clone();
                s.spawn(move || {
                    let log = AuditLog::with_path(path);
                    for i in 0..10 {
                        log.append(AuditEvent::TickStarted {
                            agents_scanned: t * 100 + i,
                        })
                        .unwrap();
                    }
                });
            }
        });

        let log = AuditLog::with_path(path.clone());
        assert_eq!(log.iter_entries().unwrap().len(), 40);
        assert!(
            log.verify_chain().unwrap().is_none(),
            "concurrent appends forked the chain"
        );
        assert!(
            path.with_extension("log.lock").exists(),
            "lock file must survive the append"
        );
    }

    // --- rotation --------------------------------------------------------

    /// Append until the log has rotated `times` times. Returns the log.
    fn rotated_log(dir: &Path, times: usize) -> AuditLog {
        let log = AuditLog::with_path(dir.join("audit.log")).with_max_bytes(2048);
        let mut seen = 0;
        let mut i = 0u32;
        while seen < times {
            let before = log.segments().unwrap().len();
            log.append(AuditEvent::TickStarted { agents_scanned: i })
                .unwrap();
            i += 1;
            seen += log.segments().unwrap().len() - before;
            assert!(i < 10_000, "log never rotated");
        }
        log
    }

    fn seam_of(log: &AuditLog) -> (String, usize, String) {
        match &log.iter_entries().unwrap()[0].event {
            AuditEvent::AuditLogRotated {
                rotated_to,
                entries,
                tail_hash,
                ..
            } => (rotated_to.clone(), *entries, tail_hash.clone()),
            other => panic!("expected a seam, got {other:?}"),
        }
    }

    #[test]
    fn nothing_rotates_below_the_threshold() {
        let dir = tempdir().unwrap();
        let log = AuditLog::with_path(dir.path().join("audit.log")).with_max_bytes(1 << 30);
        for i in 0..50 {
            log.append(AuditEvent::TickStarted { agents_scanned: i })
                .unwrap();
        }
        assert!(log.segments().unwrap().is_empty());
        assert_eq!(
            log.verify_chain_status(VerifyScope::Full).unwrap(),
            ChainStatus::IntactFromGenesis
        );
    }

    #[test]
    fn rotation_seams_the_chain_across_the_rename() {
        let dir = tempdir().unwrap();
        let log = rotated_log(dir.path(), 1);

        let segments = log.segments().unwrap();
        assert_eq!(segments.len(), 1);

        let (rotated_to, entries, tail_hash) = seam_of(&log);
        assert_eq!(
            segments[0].file_name().unwrap().to_str().unwrap(),
            rotated_to
        );

        // The seam's prev_hash is the segment's tail hash: no gap.
        let head = &log.iter_entries().unwrap()[0];
        assert_eq!(head.prev_hash, tail_hash);
        assert_eq!(
            hash_line(&read_tail(&segments[0]).unwrap().last_line.unwrap()),
            tail_hash
        );
        assert_eq!(read_entries(&segments[0]).unwrap().len(), entries);
        assert_eq!(head.severity, Severity::Notice);

        assert!(log.verify_chain().unwrap().is_none());
        assert_eq!(
            log.verify_chain_status(VerifyScope::Full).unwrap(),
            ChainStatus::IntactFromGenesis
        );
    }

    #[test]
    fn full_verify_walks_back_through_several_segments() {
        let dir = tempdir().unwrap();
        let log = rotated_log(dir.path(), 3);
        assert_eq!(log.segments().unwrap().len(), 3);
        assert_eq!(
            log.verify_chain_status(VerifyScope::Full).unwrap(),
            ChainStatus::IntactFromGenesis
        );
    }

    #[test]
    fn current_scope_stops_at_the_seam_and_says_so() {
        let dir = tempdir().unwrap();
        let log = rotated_log(dir.path(), 1);
        let (rotated_to, entries, _) = seam_of(&log);

        match log
            .verify_chain_status(VerifyScope::CurrentSegment)
            .unwrap()
        {
            ChainStatus::IntactSinceRotation {
                segment,
                entries_before,
                segment_present,
                ..
            } => {
                assert_eq!(segment, rotated_to);
                assert_eq!(entries_before, entries);
                assert!(segment_present, "the segment is right there on disk");
            }
            other => panic!("expected IntactSinceRotation, got {other:?}"),
        }
    }

    #[test]
    fn a_deleted_segment_reads_as_retention_not_tampering() {
        // The distinction this whole design exists for. Removing a whole
        // segment leaves the seam behind, still covered by the chain, so
        // verification can name what went away instead of crying tamper.
        let dir = tempdir().unwrap();
        let log = rotated_log(dir.path(), 1);
        let (rotated_to, entries, _) = seam_of(&log);
        std::fs::remove_file(dir.path().join(&rotated_to)).unwrap();

        match log.verify_chain_status(VerifyScope::Full).unwrap() {
            ChainStatus::IntactSinceRotation {
                segment,
                entries_before,
                segment_present,
                since_ts,
            } => {
                assert_eq!(segment, rotated_to);
                assert_eq!(entries_before, entries);
                assert!(!segment_present);
                assert!(!since_ts.is_empty());
            }
            other => panic!("expected IntactSinceRotation, got {other:?}"),
        }
        // And the yes/no API the daemon uses must stay quiet: a pruned log is
        // not a broken one.
        assert!(log.verify_chain().unwrap().is_none());
    }

    #[test]
    fn beheading_the_live_file_reads_as_unexplained_truncation() {
        // Same shape on disk as retention — a first entry that does not chain
        // to GENESIS — but the seam went with the deleted lines, so nothing
        // accounts for the hash it points at.
        let dir = tempdir().unwrap();
        let path = dir.path().join("audit.log");
        let log = rotated_log(dir.path(), 1);
        for _ in 0..5 {
            log.append(AuditEvent::TickStarted { agents_scanned: 9 })
                .unwrap();
        }

        let raw = std::fs::read_to_string(&path).unwrap();
        let kept: Vec<&str> = raw.lines().skip(3).collect();
        std::fs::write(&path, kept.join("\n") + "\n").unwrap();

        match log.verify_chain_status(VerifyScope::Full).unwrap() {
            ChainStatus::UnexplainedTruncation { orphan_prev_hash } => {
                assert!(!orphan_prev_hash.is_empty());
                assert_ne!(orphan_prev_hash, GENESIS_HASH);
            }
            other => panic!("expected UnexplainedTruncation, got {other:?}"),
        }
        // The daemon's yes/no call must fire here.
        let brk = log.verify_chain().unwrap().expect("must report a break");
        assert_eq!(brk.position, 0);
        assert_eq!(brk.expected, GENESIS_HASH);
    }

    #[test]
    fn tampering_inside_a_rotated_segment_is_caught_by_full_verify() {
        let dir = tempdir().unwrap();
        let log = rotated_log(dir.path(), 1);
        let segment = log.segments().unwrap().remove(0);

        let raw = std::fs::read_to_string(&segment).unwrap();
        let mut lines: Vec<String> = raw.lines().map(String::from).collect();
        let victim = lines.len() / 2;
        lines[victim] = lines[victim].replace("\"agents_scanned\":", "\"agents_scanned\":9");
        std::fs::write(&segment, lines.join("\n") + "\n").unwrap();

        match log.verify_chain_status(VerifyScope::Full).unwrap() {
            ChainStatus::Broken(brk) => {
                assert_eq!(brk.position, victim + 1);
                assert_eq!(
                    brk.segment.as_deref(),
                    segment.file_name().unwrap().to_str()
                );
            }
            other => panic!("expected Broken, got {other:?}"),
        }
    }

    #[test]
    fn truncating_a_segments_tail_breaks_the_seam_link() {
        // Cutting the *end* off a segment leaves its internal links fine. Only
        // the seam's declared tail_hash catches it.
        let dir = tempdir().unwrap();
        let log = rotated_log(dir.path(), 1);
        let segment = log.segments().unwrap().remove(0);
        let (rotated_to, _, tail_hash) = seam_of(&log);

        let raw = std::fs::read_to_string(&segment).unwrap();
        let mut lines: Vec<&str> = raw.lines().collect();
        lines.pop();
        std::fs::write(&segment, lines.join("\n") + "\n").unwrap();

        match log.verify_chain_status(VerifyScope::Full).unwrap() {
            ChainStatus::Broken(brk) => {
                assert_eq!(brk.expected, tail_hash);
                assert_ne!(brk.actual, tail_hash);
                assert_eq!(brk.segment.as_deref(), Some(rotated_to.as_str()));
            }
            other => panic!("expected Broken, got {other:?}"),
        }
    }

    #[test]
    fn an_emptied_segment_is_a_break_not_retention() {
        // Deleting the file is retention; leaving an empty one in its place is
        // somebody covering tracks.
        let dir = tempdir().unwrap();
        let log = rotated_log(dir.path(), 1);
        let segment = log.segments().unwrap().remove(0);
        std::fs::write(&segment, b"").unwrap();

        assert!(matches!(
            log.verify_chain_status(VerifyScope::Full).unwrap(),
            ChainStatus::Broken(_)
        ));
    }

    #[test]
    fn a_seam_whose_hash_does_not_match_is_not_a_seam() {
        let entry = AuditEntry {
            ts: "2026-08-06T10:00:00-0300".into(),
            severity: Severity::Notice,
            event: AuditEvent::AuditLogRotated {
                rotated_to: "audit.log.20260806T101500".into(),
                entries: 10,
                first_ts: "2026-08-01T00:00:00-0300".into(),
                tail_hash: "aaaa".into(),
            },
            prev_hash: "bbbb".into(), // disagrees with tail_hash
        };
        assert!(matches!(
            classify_head(&entry, Path::new("/x/audit.log")),
            Head::Orphan(_)
        ));
    }

    #[test]
    fn a_seam_pointing_outside_the_directory_is_refused() {
        for evil in [
            "../../etc/passwd",
            "/etc/passwd",
            "audit.log.lock",
            "audit.log.notatimestamp",
            "other.log.20260806T101500",
        ] {
            let entry = AuditEntry {
                ts: "2026-08-06T10:00:00-0300".into(),
                severity: Severity::Notice,
                event: AuditEvent::AuditLogRotated {
                    rotated_to: evil.into(),
                    entries: 1,
                    first_ts: "x".into(),
                    tail_hash: "aaaa".into(),
                },
                prev_hash: "aaaa".into(),
            };
            assert!(
                matches!(
                    classify_head(&entry, Path::new("/x/audit.log")),
                    Head::Orphan(_)
                ),
                "{evil} must not be followed"
            );
        }
    }

    #[test]
    fn segment_names_are_recognised_strictly() {
        assert!(is_segment_name("audit.log", "audit.log.20260806T101500"));
        assert!(is_segment_name("audit.log", "audit.log.20260806T101500-3"));
        assert!(!is_segment_name("audit.log", "audit.log"));
        assert!(!is_segment_name("audit.log", "audit.log.lock"));
        assert!(!is_segment_name("audit.log", "audit.log.20260806T10150"));
        assert!(!is_segment_name("audit.log", "audit.log.2026080610150x"));
        assert!(!is_segment_name("audit.log", "audit.log.20260806T101500-"));
        assert!(!is_segment_name("audit.log", "audit.log.20260806T101500-x"));
        assert!(!is_segment_name(
            "audit.log",
            "../audit.log.20260806T101500"
        ));
    }

    #[test]
    fn segments_are_listed_oldest_first_and_exclude_the_lock_file() {
        let dir = tempdir().unwrap();
        let log = rotated_log(dir.path(), 2);
        let names: Vec<String> = log
            .segments()
            .unwrap()
            .iter()
            .map(|p| p.file_name().unwrap().to_str().unwrap().to_string())
            .collect();
        assert_eq!(names.len(), 2);
        assert!(names.windows(2).all(|w| w[0] < w[1]));
        assert!(dir.path().join("audit.log.lock").exists());
        assert!(!names.iter().any(|n| n.ends_with(".lock")));
    }

    #[test]
    fn appending_after_rotation_keeps_chaining_onto_the_seam() {
        let dir = tempdir().unwrap();
        let log = rotated_log(dir.path(), 1);
        let entries = log.iter_entries().unwrap();

        // The append that tripped rotation writes its own entry right after
        // the seam, chained onto it — the rename is invisible to the chain.
        assert_eq!(entries.len(), 2, "seam plus the event that tripped it");
        let seam_line = serde_json::to_string(&entries[0]).unwrap();
        assert_eq!(entries[1].prev_hash, hash_line(&seam_line));

        let next = log
            .append(AuditEvent::TickStarted { agents_scanned: 77 })
            .unwrap();
        assert_eq!(
            next.prev_hash,
            hash_line(&serde_json::to_string(&entries[1]).unwrap())
        );
        assert!(log.verify_chain().unwrap().is_none());
    }
}
