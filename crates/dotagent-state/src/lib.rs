//! Filesystem-backed state store.
//!
//! All files live under `~/.config/dotagent/` (or `$DOTAGENT_HOME`). See
//! [`paths`] for the exact layout.
//!
//! Heartbeat and window state use atomic writes (write-to-temp + rename) and
//! `flock` to guard against concurrent ticks racing.

pub mod audit;
pub mod manifest_cache;
pub mod paths;

pub use audit::{AuditError, AuditLog, ChainBreak};
pub use manifest_cache::{hash_manifest_file, KnownManifest, KnownManifests, ManifestCache};

use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Local, TimeZone};
use dotagent_core::{Heartbeat, WindowState};
use fs2::FileExt;
use serde::de::DeserializeOwned;
use serde::Serialize;
use thiserror::Error;
use tracing::warn;

pub type Result<T> = std::result::Result<T, StateError>;

#[derive(Debug, Error)]
pub enum StateError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("json: {0}")]
    Json(#[from] serde_json::Error),
    #[error("home directory not found")]
    NoHome,
}

/// Slug derivation matches the legacy `agent_slug_from_args` in
/// `lib/agent.fish`: strip leading dashes, lower, snake-case, default "default".
pub fn slug_from_args(args: &[String]) -> String {
    if args.is_empty() {
        return "default".into();
    }
    let mut parts: Vec<String> = Vec::with_capacity(args.len());
    for arg in args {
        let cleaned = arg.trim_start_matches('-').to_string();
        if !cleaned.is_empty() {
            parts.push(cleaned);
        }
    }
    if parts.is_empty() {
        return "default".into();
    }
    let joined = parts.join("_").to_lowercase();
    let mut slug: String = joined
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect();
    while slug.contains("__") {
        slug = slug.replace("__", "_");
    }
    let trimmed = slug.trim_matches('_').to_string();
    if trimmed.is_empty() {
        "default".into()
    } else {
        trimmed
    }
}

/// Resolve the on-disk paths for state files. Wraps `~/.local/state/dotagent`
/// so tests can inject a tempdir via [`StateStore::with_root`].
#[derive(Debug, Clone)]
pub struct StateStore {
    root: PathBuf,
}

impl StateStore {
    /// Open the state store under the dotagent state directory
    /// (`$DOTAGENT_HOME/state/`).
    pub fn from_home() -> Result<Self> {
        Ok(Self::with_root(paths::state_dir()))
    }

    pub fn with_root(root: PathBuf) -> Self {
        Self { root }
    }

    pub fn heartbeat_path(&self, agent: &str, slug: &str) -> PathBuf {
        self.root
            .join("agents")
            .join(agent)
            .join(format!("{slug}.heartbeat.json"))
    }

    pub fn window_path(&self, agent: &str, slug: &str, expected_at: DateTime<Local>) -> PathBuf {
        let label = expected_at.format("%Y-%m-%d-%H%M").to_string();
        self.root
            .join("windows")
            .join(format!("{agent}-{slug}-{label}.json"))
    }

    pub fn read_heartbeat(&self, agent: &str, slug: &str) -> Result<Option<Heartbeat>> {
        read_json(self.heartbeat_path(agent, slug))
    }

    pub fn write_heartbeat(&self, hb: &Heartbeat) -> Result<()> {
        let path = self.heartbeat_path(&hb.name, &hb.slug);
        write_json(&path, hb)
    }

    pub fn read_window(
        &self,
        agent: &str,
        slug: &str,
        expected_at: DateTime<Local>,
    ) -> Result<Option<WindowState>> {
        read_json(self.window_path(agent, slug, expected_at))
    }

    pub fn write_window(
        &self,
        ws: &WindowState,
        slug: &str,
        expected_at: DateTime<Local>,
    ) -> Result<()> {
        let path = self.window_path(&ws.agent, slug, expected_at);
        write_json(&path, ws)
    }
}

fn read_json<T: DeserializeOwned>(path: PathBuf) -> Result<Option<T>> {
    if !path.exists() {
        return Ok(None);
    }
    let f = File::open(&path)?;
    let val: T = serde_json::from_reader(f)?;
    Ok(Some(val))
}

fn write_json<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let lock_path = path.with_extension("lock");
    let lock = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .read(true)
        .open(&lock_path)?;
    lock.lock_exclusive()?;

    let tmp = path.with_extension("json.tmp");
    {
        let mut f = File::create(&tmp)?;
        let bytes = serde_json::to_vec_pretty(value)?;
        f.write_all(&bytes)?;
        f.sync_all()?;
    }
    fs::rename(&tmp, path)?;

    // best-effort: drop lock file when done
    if let Err(e) = fs::remove_file(&lock_path) {
        warn!(?e, "failed to remove lock file");
    }
    drop(lock);
    Ok(())
}

/// Helper to convert epoch → `DateTime<Local>` for callers that store time as
/// `i64` (matches the legacy heartbeat shape).
pub fn epoch_to_local(epoch: i64) -> Option<DateTime<Local>> {
    Local.timestamp_opt(epoch, 0).single()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn slug_default_for_empty_args() {
        assert_eq!(slug_from_args(&[]), "default");
    }

    #[test]
    fn slug_strips_leading_dashes_and_snake_cases() {
        let args = vec!["--period".into(), "dia-anterior".into()];
        assert_eq!(slug_from_args(&args), "period_dia-anterior");
    }

    #[test]
    fn heartbeat_roundtrip() {
        let dir = tempdir().unwrap();
        let store = StateStore::with_root(dir.path().to_path_buf());
        let hb = Heartbeat {
            name: "x".into(),
            slug: "default".into(),
            args: vec![],
            started_at: 1700000000,
            started_at_iso: "2023-11-14T22:13:20+0000".into(),
            finished_at: Some(1700000100),
            finished_at_iso: Some("2023-11-14T22:15:00+0000".into()),
            exit_code: Some(0),
            duration_seconds: Some(100),
            last_success_at: Some(1700000100),
            last_success_at_iso: Some("2023-11-14T22:15:00+0000".into()),
        };
        store.write_heartbeat(&hb).unwrap();
        let got = store.read_heartbeat("x", "default").unwrap().unwrap();
        assert_eq!(got.exit_code, Some(0));
    }
}
