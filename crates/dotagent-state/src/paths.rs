//! Centralized filesystem layout for dotagent.
//!
//! Everything dotagent writes lives under a single root — by default
//! `~/.config/dotagent/`. The user can override with `DOTAGENT_HOME`.
//!
//! ```text
//! $DOTAGENT_HOME/                 # default: ~/.config/dotagent
//!   agents/<name>/                # manifests (or symlinks to them)
//!   plugins/                      # custom plugin binaries
//!   state/
//!     agents/<name>/<slug>.heartbeat.json
//!     windows/<agent>-<slug>-<YYYY-MM-DD-HHMM>.json
//!     plugins/<plugin-name>/<key>.json
//!     known_manifests.json
//!     daemon.pid
//!   logs/                         # daemon-captured stdout/stderr
//!   audit.log                     # append-only hash-chained event log
//! ```
//!
//! Centralization is a deliberate trade-off vs. the XDG Base Directory
//! Spec (`~/.config` for config, `~/.local/state` for state,
//! `~/.local/share` for data, `~/.cache` for cache). XDG is great for
//! backup tooling that wants to selectively skip caches, but it scatters
//! a single user's dotagent surface across four directories — hard to
//! find, hard to inspect, hard to wipe atomically. dotagent picks
//! convergence over spec.

use std::path::PathBuf;

/// Returns the dotagent root. Resolution order:
///
/// 1. `DOTAGENT_HOME` env var (absolute path)
/// 2. `$HOME/.config/dotagent`
///
/// Panics is impossible — if `$HOME` is not set, returns `./.dotagent`
/// as a last-resort sentinel so callers don't have to deal with `None`.
pub fn home() -> PathBuf {
    if let Ok(p) = std::env::var("DOTAGENT_HOME") {
        if !p.is_empty() {
            return PathBuf::from(p);
        }
    }
    if let Some(h) = dirs::home_dir() {
        return h.join(".config/dotagent");
    }
    PathBuf::from(".dotagent")
}

pub fn agents_dir() -> PathBuf {
    home().join("agents")
}

pub fn plugins_dir() -> PathBuf {
    home().join("plugins")
}

pub fn state_dir() -> PathBuf {
    home().join("state")
}

pub fn state_agents_dir() -> PathBuf {
    state_dir().join("agents")
}

pub fn state_windows_dir() -> PathBuf {
    state_dir().join("windows")
}

pub fn state_plugins_dir() -> PathBuf {
    state_dir().join("plugins")
}

pub fn known_manifests_file() -> PathBuf {
    state_dir().join("known_manifests.json")
}

pub fn daemon_pid_file() -> PathBuf {
    state_dir().join("daemon.pid")
}

pub fn logs_dir() -> PathBuf {
    home().join("logs")
}

/// Daemon's own log directory: `logs/daemon/`.
pub fn daemon_logs_dir() -> PathBuf {
    logs_dir().join("daemon")
}

/// Per-agent log directory: `logs/agents/<name>/`.
pub fn agent_logs_dir(agent: &str) -> PathBuf {
    logs_dir().join("agents").join(agent)
}

/// Per-plugin log directory: `logs/plugins/<name>/`.
pub fn plugin_logs_dir(plugin: &str) -> PathBuf {
    logs_dir().join("plugins").join(plugin)
}

/// Global config file: `$DOTAGENT_HOME/config.toml`.
pub fn config_file() -> PathBuf {
    home().join("config.toml")
}

pub fn audit_log_file() -> PathBuf {
    home().join("audit.log")
}

/// Per-plugin state directory (e.g., `state/plugins/notify-imessage/`).
pub fn plugin_state_dir(plugin_name: &str) -> PathBuf {
    state_plugins_dir().join(plugin_name)
}
