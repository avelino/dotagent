//! Generate the SINGLE unit file that launches the dotagent daemon.
//!
//! Architectural note: dotagent runs as ONE long-lived daemon
//! (`dotagent daemon`) that manages every scheduled agent internally. We
//! do NOT generate one unit per agent — the daemon's adaptive scheduler
//! sleeps until the next event and dispatches it. The user installs and
//! enables a single unit; everything else flows from that.
//!
//! - **macOS**: `~/Library/LaunchAgents/run.avelino.dotagent.plist`
//!   - `KeepAlive=true` (restart if daemon crashes)
//!   - `RunAtLoad=true` (start at user login)
//! - **Linux**: `~/.config/systemd/user/run.avelino.dotagent.service`
//!   - `Restart=always`

pub mod launchd;
pub mod systemd;
pub mod template;

use std::path::PathBuf;

use thiserror::Error;

pub type Result<T> = std::result::Result<T, UnitGenError>;

#[derive(Debug, Error)]
pub enum UnitGenError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("unsupported platform: {0}")]
    Unsupported(String),
    #[error("missing home directory")]
    NoHome,
}

/// Label used for the launchd plist / systemd unit. Hard-coded by design —
/// the daemon is conceptually a singleton.
pub const DAEMON_LABEL: &str = "run.avelino.dotagent";

/// Where the generated unit file lands.
#[derive(Debug, Clone)]
pub struct UnitPath {
    pub path: PathBuf,
}

/// Inputs to unit generation.
#[derive(Debug, Clone)]
pub struct GenContext {
    pub dotagent_binary: PathBuf,
    pub log_dir: PathBuf,
}

/// Generate the daemon unit for this platform.
pub fn generate_daemon_unit(ctx: &GenContext) -> Result<UnitPath> {
    #[cfg(target_os = "macos")]
    return launchd::generate_daemon(ctx);

    #[cfg(target_os = "linux")]
    return systemd::generate_daemon(ctx);

    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    Err(UnitGenError::Unsupported(std::env::consts::OS.into()))
}

/// Remove the daemon unit for this platform. Idempotent.
pub fn uninstall_daemon_unit() -> Result<Option<PathBuf>> {
    let home = dirs::home_dir().ok_or(UnitGenError::NoHome)?;

    #[cfg(target_os = "macos")]
    let path = home.join(format!("Library/LaunchAgents/{DAEMON_LABEL}.plist"));

    #[cfg(target_os = "linux")]
    let path = home.join(format!(".config/systemd/user/{DAEMON_LABEL}.service"));

    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    let path: std::path::PathBuf = home.join(".dotagent-unit-not-supported");

    if path.is_file() {
        std::fs::remove_file(&path)?;
        Ok(Some(path))
    } else {
        Ok(None)
    }
}
