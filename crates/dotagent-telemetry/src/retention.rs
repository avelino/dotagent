//! Log retention sweeper.
//!
//! Walks `logs/daemon/` and `logs/agents/<name>/`:
//! - Files older than `compress_after_days` (default 1) are gzipped.
//! - Files older than the configured retention horizon are deleted.
//!
//! Safe to call frequently — uses `mtime` so it's idempotent.

use std::path::{Path, PathBuf};
use std::time::SystemTime;

use chrono::Local;
use dotagent_core::config::LoggingConfig;
use flate2::write::GzEncoder;
use flate2::Compression;
use tracing::{debug, warn};

#[derive(Debug, Default, Clone)]
pub struct SweepStats {
    pub compressed: u32,
    pub deleted: u32,
    pub scanned: u32,
}

/// Run a single retention pass across daemon + every agent log dir.
pub fn sweep_all(logging: &LoggingConfig) -> SweepStats {
    let mut stats = SweepStats::default();

    let daemon_dir = dotagent_state::paths::daemon_logs_dir();
    sweep_dir(
        &daemon_dir,
        logging.compress_after_days,
        logging.retention_days,
        &mut stats,
    );

    // logs/agents/*  → per-agent retention horizon
    let agents_root = dotagent_state::paths::logs_dir().join("agents");
    if let Ok(entries) = std::fs::read_dir(&agents_root) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                sweep_dir(
                    &path,
                    logging.compress_after_days,
                    logging.per_agent_retention_days,
                    &mut stats,
                );
            }
        }
    }
    stats
}

fn sweep_dir(dir: &Path, compress_after_days: u32, retention_days: u32, stats: &mut SweepStats) {
    let now = Local::now();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };

    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        stats.scanned += 1;
        let Ok(metadata) = entry.metadata() else {
            continue;
        };
        let Ok(mtime) = metadata.modified() else {
            continue;
        };
        let age_days = age_in_days(now.into(), mtime);

        // Already gzipped → check delete only.
        let is_gz = path.extension().is_some_and(|e| e == "gz");

        // Never touch the currently-active log file (no date suffix).
        let is_active = path
            .file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| !n.contains('.') || matches!(n.rsplit_once('.'), Some((_, "log"))));

        if age_days as u32 > retention_days {
            if let Err(e) = std::fs::remove_file(&path) {
                warn!(error = %e, ?path, "failed to delete aged-out log");
            } else {
                stats.deleted += 1;
                debug!(?path, age_days, "deleted aged log");
            }
            continue;
        }

        if !is_gz && !is_active && age_days as u32 > compress_after_days {
            if let Err(e) = gzip_file(&path) {
                warn!(error = %e, ?path, "failed to gzip aged log");
            } else {
                stats.compressed += 1;
                debug!(?path, age_days, "compressed aged log");
            }
        }
    }
}

fn age_in_days(now: SystemTime, when: SystemTime) -> i64 {
    now.duration_since(when)
        .map(|d| (d.as_secs() / 86400) as i64)
        .unwrap_or(0)
}

fn gzip_file(path: &Path) -> std::io::Result<()> {
    let raw = std::fs::read(path)?;
    let gz_path: PathBuf = {
        let mut p = path.to_path_buf();
        let new_name = format!(
            "{}.gz",
            p.file_name().and_then(|s| s.to_str()).unwrap_or("log")
        );
        p.set_file_name(new_name);
        p
    };
    let f = std::fs::File::create(&gz_path)?;
    let mut enc = GzEncoder::new(f, Compression::default());
    use std::io::Write;
    enc.write_all(&raw)?;
    enc.finish()?;
    std::fs::remove_file(path)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn age_in_days_handles_zero() {
        let now = SystemTime::now();
        assert_eq!(age_in_days(now, now), 0);
    }

    #[test]
    fn gzip_replaces_file() {
        let dir = tempdir().unwrap();
        let f = dir.path().join("a.log.2026-05-10");
        std::fs::write(&f, "hello world").unwrap();
        gzip_file(&f).unwrap();
        assert!(!f.exists());
        assert!(dir.path().join("a.log.2026-05-10.gz").exists());
    }
}
