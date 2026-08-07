//! Retention sweeper for everything dotagent leaves on disk.
//!
//! Two families, swept on the same daily pass:
//!
//! - **Logs** — `logs/daemon/` and `logs/agents/<name>/`. Files older than
//!   `compress_after_days` (default 1) are gzipped; files older than the
//!   retention horizon are deleted.
//! - **Windows** — `state/windows/`. Deleted only, never compressed: the
//!   daemon reads these files as JSON, and a gzipped window is a corrupted
//!   window.
//!
//! Safe to call frequently — decisions are made on `mtime`, so it's idempotent.

use std::fs::OpenOptions;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use chrono::Local;
use dotagent_core::config::{LoggingConfig, StateConfig};
use flate2::write::GzEncoder;
use flate2::Compression;
use fs2::FileExt;
use tracing::{debug, warn};

#[derive(Debug, Default, Clone)]
pub struct SweepStats {
    pub compressed: u32,
    pub deleted: u32,
    pub scanned: u32,
    /// Window files (`state/windows/*.json`) examined.
    pub windows_scanned: u32,
    /// Windows removed. Counted per window, not per file — the `.lock` that
    /// goes with it is not counted twice.
    pub windows_deleted: u32,
}

/// Run a single retention pass with the default state horizon.
///
/// Kept for callers that only carry a [`LoggingConfig`]. Prefer
/// [`sweep_all_with`], which honors `[state]` from `config.toml`.
pub fn sweep_all(logging: &LoggingConfig) -> SweepStats {
    sweep_all_with(logging, &StateConfig::default())
}

/// Run a single retention pass across daemon logs, every agent log dir, and
/// `state/windows/`.
pub fn sweep_all_with(logging: &LoggingConfig, state: &StateConfig) -> SweepStats {
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

    sweep_windows_dir(
        &dotagent_state::paths::state_windows_dir(),
        state.window_retention_days,
        SystemTime::now(),
        &mut stats,
    );

    stats
}

/// Delete aged-out window state, each `.json` together with its `.lock`.
///
/// **Age comes from `mtime`, not from the timestamp in the filename.** The two
/// disagree exactly where it matters: a window named for 03:00 that a retry
/// touched at 05:00 is two hours old by `mtime` and older by name. `mtime` is
/// the conservative reading — it can only ever make a file look *younger* than
/// its name, and looking younger means surviving. Reading the name would let a
/// window still under retry age out mid-retry.
///
/// `retention_days == 0` disables the sweep.
fn sweep_windows_dir(dir: &Path, retention_days: u32, now: SystemTime, stats: &mut SweepStats) {
    if retention_days == 0 {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return; // no windows yet, or no dotagent home — nothing to do
    };

    // Snapshot before unlinking. Iterating a directory while removing entries
    // from it is not guaranteed to visit everything.
    let files: Vec<PathBuf> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_file())
        .collect();

    for path in files {
        let is_window = path.extension().is_some_and(|e| e == "json");
        if !is_window {
            // `.lock` and `.json.tmp` are removed alongside their window. One
            // reaching here has no window left, so it ages out on its own.
            if window_for_sidecar(&path).exists() {
                continue;
            }
            if !is_aged_out(&path, retention_days, now) {
                continue;
            }
            let gone = if path.extension().is_some_and(|e| e == "lock") {
                delete_orphan_lock(&path)
            } else {
                remove(&path)
            };
            if gone {
                stats.windows_deleted += 1;
            }
            continue;
        }

        stats.windows_scanned += 1;
        if !is_aged_out(&path, retention_days, now) {
            continue;
        }
        if delete_window(&path) {
            stats.windows_deleted += 1;
            debug!(?path, "deleted aged window state");
        }
    }
}

/// The window a `.lock` / `.json.tmp` belongs to.
///
/// Mirrors `StateStore::write_json`, which derives both sidecar names with
/// `with_extension` — so the inverse has to use it too, or an agent name
/// containing a dot would resolve to a different file than the writer used.
fn window_for_sidecar(path: &Path) -> PathBuf {
    path.with_extension("json")
}

/// Remove a window and its `.lock`, refusing while a writer holds the lock.
///
/// Returns whether the window is gone.
///
/// Unlinking a lock file is normally forbidden (see the comment in
/// `dotagent_state::write_json`: a fresh inode hands two writers their own
/// "exclusive" lock). It is safe here only because the pair is being retired
/// wholesale, and only for a window whose horizon the daemon stopped
/// consulting weeks ago. The `try_lock` below is the belt to that suspenders.
fn delete_window(json: &Path) -> bool {
    let lock_path = json.with_extension("lock");
    if !lock_path.exists() {
        return remove(json);
    }
    let Some(lock) = try_hold(&lock_path) else {
        debug!(?json, "window is locked by a live writer, leaving it alone");
        return false;
    };
    let removed = remove(json);
    if removed {
        remove(&lock_path);
    }
    drop(lock); // releases the flock
    removed
}

/// A `.lock` whose window is already gone — a crash between `create(lock)` and
/// `rename(tmp, json)` leaves one behind, and nothing else reclaims it.
fn delete_orphan_lock(lock_path: &Path) -> bool {
    let Some(lock) = try_hold(lock_path) else {
        return false;
    };
    let removed = remove(lock_path);
    drop(lock);
    removed
}

/// Take the exclusive flock, or `None` if a writer holds it (or the file can't
/// be opened for locking at all — in which case we can't prove it's idle, so we
/// don't touch it).
fn try_hold(lock_path: &Path) -> Option<std::fs::File> {
    let f = OpenOptions::new()
        .read(true)
        .write(true)
        .open(lock_path)
        .ok()?;
    f.try_lock_exclusive().ok()?;
    Some(f)
}

fn remove(path: &Path) -> bool {
    match std::fs::remove_file(path) {
        Ok(()) => true,
        Err(e) => {
            warn!(error = %e, ?path, "failed to delete aged-out state file");
            false
        }
    }
}

fn is_aged_out(path: &Path, retention_days: u32, now: SystemTime) -> bool {
    let Ok(metadata) = std::fs::metadata(path) else {
        return false;
    };
    let Ok(mtime) = metadata.modified() else {
        return false;
    };
    age_in_days(now, mtime) as u32 > retention_days
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

        // An un-rotated log is never deleted, however idle it looks.
        //
        // launchd/systemd hold the fd for `run.avelino.dotagent-error.log`
        // open for the life of the daemon, and that file only receives what
        // dies before tracing exists — a panic, an abort, a dyld failure. It
        // can go a month without a byte and still be live. Unlinking it
        // reclaims nothing (the fd pins the inode) and silently redirects the
        // next crash into a dangling inode, losing exactly the diagnostic the
        // file exists for.
        //
        // Accepted cost: an `<agent>.log` left behind by a deleted agent never
        // expires on age. It goes when its directory does.
        if !is_active && age_days as u32 > retention_days {
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
    use std::time::Duration;
    use tempfile::tempdir;

    const DAY: u64 = 86_400;

    fn days_ago(n: u64) -> SystemTime {
        SystemTime::now() - Duration::from_secs(n * DAY)
    }

    fn age_file(path: &Path, n: u64) {
        let f = OpenOptions::new().write(true).open(path).unwrap();
        f.set_times(std::fs::FileTimes::new().set_modified(days_ago(n)))
            .unwrap();
    }

    /// A window as `StateStore::write_json` leaves it: `.json` + `.lock`.
    fn write_window(dir: &Path, name: &str, age_days: u64) -> (PathBuf, PathBuf) {
        let json = dir.join(format!("{name}.json"));
        let lock = dir.join(format!("{name}.lock"));
        std::fs::write(&json, r#"{"agent":"x","attempts":2}"#).unwrap();
        std::fs::write(&lock, "").unwrap();
        age_file(&json, age_days);
        age_file(&lock, age_days);
        (json, lock)
    }

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

    // --- windows ---

    #[test]
    fn recent_window_survives_with_its_lock() {
        let dir = tempdir().unwrap();
        let (json, lock) = write_window(dir.path(), "hello-default-2026-08-04-0300", 3);

        let mut stats = SweepStats::default();
        sweep_windows_dir(dir.path(), 30, SystemTime::now(), &mut stats);

        assert!(
            json.exists(),
            "a 3-day-old window is nowhere near the horizon"
        );
        assert!(lock.exists());
        assert_eq!(stats.windows_scanned, 1);
        assert_eq!(stats.windows_deleted, 0);
    }

    #[test]
    fn window_exactly_at_the_horizon_survives() {
        // `>` not `>=`, same as the log sweeper. Off-by-one here costs a day
        // of retry state, so pin it.
        let dir = tempdir().unwrap();
        let (json, _) = write_window(dir.path(), "hello-default-2026-07-07-0300", 30);

        let mut stats = SweepStats::default();
        sweep_windows_dir(dir.path(), 30, SystemTime::now(), &mut stats);

        assert!(json.exists());
        assert_eq!(stats.windows_deleted, 0);
    }

    #[test]
    fn aged_window_dies_together_with_its_lock() {
        let dir = tempdir().unwrap();
        let (json, lock) = write_window(
            dir.path(),
            "calendar-to-webhook-default-2026-05-19-1200",
            45,
        );

        let mut stats = SweepStats::default();
        sweep_windows_dir(dir.path(), 30, SystemTime::now(), &mut stats);

        assert!(!json.exists(), "aged window must go");
        assert!(
            !lock.exists(),
            "orphan lock files are the other half of the leak"
        );
        assert_eq!(stats.windows_scanned, 1);
        assert_eq!(stats.windows_deleted, 1, "the pair counts as one window");
    }

    #[test]
    fn a_window_a_writer_holds_is_never_deleted() {
        // The lock is the only evidence a retry is mid-write. flock is
        // per-open-file-description, so a second handle in this same process
        // is exactly what the daemon's writer looks like from here.
        let dir = tempdir().unwrap();
        let (json, lock) = write_window(dir.path(), "busy-default-2026-05-19-1200", 45);

        let held = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&lock)
            .unwrap();
        held.lock_exclusive().unwrap();

        let mut stats = SweepStats::default();
        sweep_windows_dir(dir.path(), 30, SystemTime::now(), &mut stats);

        assert!(json.exists(), "must not pull state out from under a writer");
        assert!(lock.exists());
        assert_eq!(stats.windows_deleted, 0);
        drop(held);
    }

    #[test]
    fn windows_are_deleted_never_compressed() {
        // A gzipped window is a corrupted window — the daemon reads it as JSON.
        let dir = tempdir().unwrap();
        let (json, _) = write_window(dir.path(), "hello-default-2026-08-01-0300", 5);

        let mut stats = SweepStats::default();
        sweep_windows_dir(dir.path(), 30, SystemTime::now(), &mut stats);

        assert!(json.exists());
        assert_eq!(stats.compressed, 0);
        assert!(!dir
            .path()
            .join("hello-default-2026-08-01-0300.json.gz")
            .exists());
    }

    #[test]
    fn zero_retention_keeps_every_window() {
        let dir = tempdir().unwrap();
        let (json, lock) = write_window(dir.path(), "ancient-default-2020-01-01-0000", 2000);

        let mut stats = SweepStats::default();
        sweep_windows_dir(dir.path(), 0, SystemTime::now(), &mut stats);

        assert!(json.exists());
        assert!(lock.exists());
        assert_eq!(
            stats.windows_scanned, 0,
            "opted out, so nothing is even read"
        );
    }

    #[test]
    fn missing_windows_dir_is_not_a_panic() {
        let dir = tempdir().unwrap();
        let mut stats = SweepStats::default();
        sweep_windows_dir(&dir.path().join("nope"), 30, SystemTime::now(), &mut stats);
        assert_eq!(stats.windows_scanned, 0);
        assert_eq!(stats.windows_deleted, 0);
    }

    #[test]
    fn orphan_sidecars_age_out_on_their_own() {
        // A crash between `create(lock)` and `rename(tmp, json)` leaves these
        // behind. Nothing else would ever reclaim them.
        let dir = tempdir().unwrap();
        let orphan_lock = dir.path().join("crashed-default-2026-05-19-1200.lock");
        let orphan_tmp = dir.path().join("crashed-default-2026-05-19-1300.json.tmp");
        std::fs::write(&orphan_lock, "").unwrap();
        std::fs::write(&orphan_tmp, "{").unwrap();
        age_file(&orphan_lock, 45);
        age_file(&orphan_tmp, 45);

        let mut stats = SweepStats::default();
        sweep_windows_dir(dir.path(), 30, SystemTime::now(), &mut stats);

        assert!(!orphan_lock.exists());
        assert!(!orphan_tmp.exists());
        assert_eq!(stats.windows_scanned, 0, "no window was examined");
        assert_eq!(stats.windows_deleted, 2);
    }

    #[test]
    fn an_orphan_lock_a_writer_holds_is_left_alone() {
        // A writer between `create(lock)` and `rename(tmp, json)` looks exactly
        // like an orphan. It won't be aged out in practice (brand new), but the
        // flock is what makes that guarantee independent of the clock.
        let dir = tempdir().unwrap();
        let lock = dir.path().join("mid-write-default-2026-05-19-1200.lock");
        std::fs::write(&lock, "").unwrap();
        age_file(&lock, 45);

        let held = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&lock)
            .unwrap();
        held.lock_exclusive().unwrap();

        let mut stats = SweepStats::default();
        sweep_windows_dir(dir.path(), 30, SystemTime::now(), &mut stats);

        assert!(lock.exists());
        assert_eq!(stats.windows_deleted, 0);
        drop(held);
    }

    #[test]
    fn a_lock_whose_window_is_young_is_left_alone() {
        // The lock file is created once and never written again, so its mtime
        // ages past the horizon while the window it guards stays fresh.
        // Judging it on its own age would unlink a live lock.
        let dir = tempdir().unwrap();
        let json = dir.path().join("hello-default-2026-08-05-0300.json");
        let lock = dir.path().join("hello-default-2026-08-05-0300.lock");
        std::fs::write(&json, "{}").unwrap();
        std::fs::write(&lock, "").unwrap();
        age_file(&lock, 60);

        let mut stats = SweepStats::default();
        sweep_windows_dir(dir.path(), 30, SystemTime::now(), &mut stats);

        assert!(json.exists());
        assert!(
            lock.exists(),
            "a lock follows its window, not its own mtime"
        );
        assert_eq!(stats.windows_deleted, 0);
    }

    // --- logs: guard against the window work regressing the original sweep ---

    #[test]
    fn log_sweep_still_compresses_and_deletes() {
        let dir = tempdir().unwrap();
        let active = dir.path().join("daemon.log");
        let rotated = dir.path().join("daemon.log.2026-08-04");
        let ancient = dir.path().join("daemon.log.2026-05-01");
        for p in [&active, &rotated, &ancient] {
            std::fs::write(p, "line\n").unwrap();
        }
        age_file(&rotated, 3);
        age_file(&ancient, 45);

        let mut stats = SweepStats::default();
        sweep_dir(dir.path(), 1, 30, &mut stats);

        assert!(active.exists(), "the live log is never touched");
        assert!(!rotated.exists());
        assert!(dir.path().join("daemon.log.2026-08-04.gz").exists());
        assert!(!ancient.exists());
        assert_eq!(stats.compressed, 1);
        assert_eq!(stats.deleted, 1);
        assert_eq!(stats.scanned, 3);
    }

    #[test]
    fn an_idle_but_live_log_is_never_deleted() {
        // `run.avelino.dotagent-error.log` is launchd's StandardErrorPath. It
        // only receives what dies before tracing exists — a panic, an abort —
        // so it can sit untouched for months and still be open. Unlinking it
        // reclaims nothing (launchd pins the inode) and sends the next crash
        // into a dangling inode.
        let dir = tempdir().unwrap();
        let live = dir.path().join("run.avelino.dotagent-error.log");
        let rotated = dir.path().join("run.avelino.dotagent-error.log.2026-05-01");
        std::fs::write(&live, "").unwrap();
        std::fs::write(&rotated, "old panic\n").unwrap();
        age_file(&live, 400);
        age_file(&rotated, 400);

        let mut stats = SweepStats::default();
        sweep_dir(dir.path(), 1, 30, &mut stats);

        assert!(live.exists(), "the fd launchd holds outlives any horizon");
        assert!(!rotated.exists(), "rotated copies still age out normally");
        assert_eq!(stats.deleted, 1);
    }

    #[test]
    fn log_sweep_leaves_window_counters_untouched() {
        let dir = tempdir().unwrap();
        let f = dir.path().join("daemon.log.2026-05-01");
        std::fs::write(&f, "x").unwrap();
        age_file(&f, 45);

        let mut stats = SweepStats::default();
        sweep_dir(dir.path(), 1, 30, &mut stats);

        assert_eq!(stats.deleted, 1);
        assert_eq!(
            stats.windows_deleted, 0,
            "log and window counters are separate"
        );
        assert_eq!(stats.windows_scanned, 0);
    }
}
