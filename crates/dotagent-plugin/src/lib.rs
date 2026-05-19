//! Plugin client and protocol.
//!
//! Plugins are external binaries `dotagent-plugin-<name>` discovered via
//! `$DOTAGENT_PLUGIN_PATH`, `~/.config/dotagent/plugins/`,
//! `/usr/local/lib/dotagent/plugins/`, and `$PATH`. They speak a minimal
//! CLI protocol:
//!
//! ```text
//! dotagent-plugin-<name> <verb>
//! ```
//!
//! Verbs:
//! - `info`     — print JSON with metadata (name, version, kinds, schema, platforms)
//! - `validate` — read config JSON on stdin, return `{ok: true}` or `{ok: false, error: "..."}`
//! - `invoke`   — read invocation JSON on stdin, perform the action, return JSON on stdout
//!
//! Exit code: 0 = success, !=0 = failure. Stderr is for human logs.

use std::path::PathBuf;
use std::process::Stdio;

use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;
use tracing::debug;

pub type Result<T> = std::result::Result<T, PluginError>;

#[derive(Debug, Error)]
pub enum PluginError {
    #[error("plugin not found: {0}")]
    NotFound(String),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("json: {0}")]
    Json(#[from] serde_json::Error),
    #[error("plugin {plugin} failed (exit {code}): {stderr}")]
    Failed {
        plugin: String,
        code: i32,
        stderr: String,
    },
}

/// What kind of role a plugin plays.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PluginKind {
    Notify,
    Preflight,
    Sink,
}

/// Response to the `info` verb.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PluginInfo {
    pub name: String,
    #[serde(default)]
    pub version: Option<String>,
    #[serde(default)]
    pub kinds: Vec<PluginKind>,
    #[serde(default)]
    pub platforms: Vec<String>,
    #[serde(default)]
    pub schema: serde_json::Value,
}

/// Generic response shape — plugins are expected to return JSON with at least
/// an `ok` boolean. Extra fields are kept in `extra` for downstream handling.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PluginResponse {
    pub ok: bool,
    #[serde(default)]
    pub error: Option<String>,
    #[serde(flatten)]
    pub extra: serde_json::Map<String, serde_json::Value>,
}

/// Payload sent to the `invoke` verb.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InvokePayload {
    pub kind: PluginKind,
    pub agent: String,
    pub schedule: String,
    /// Event name (e.g., `attempt_failed`, `given_up`, `recovered`). For
    /// `preflight` it's `"preflight"`.
    pub event: String,
    #[serde(default)]
    pub message: Option<String>,
    /// Plugin-specific config from the manifest.
    pub config: serde_json::Value,
}

/// Resolves a plugin short name to a binary path.
pub struct PluginClient {
    search_paths: Vec<PathBuf>,
}

impl PluginClient {
    /// Build the client from the standard discovery order.
    pub fn from_environment() -> Self {
        let mut paths = Vec::new();
        if let Ok(env_path) = std::env::var("DOTAGENT_PLUGIN_PATH") {
            for p in env_path.split(':') {
                if !p.is_empty() {
                    paths.push(PathBuf::from(p));
                }
            }
        }
        if let Some(home) = dirs::home_dir() {
            paths.push(home.join(".config/dotagent/plugins"));
        }
        paths.push(PathBuf::from("/usr/local/lib/dotagent/plugins"));
        Self {
            search_paths: paths,
        }
    }

    pub fn with_search_paths(paths: Vec<PathBuf>) -> Self {
        Self {
            search_paths: paths,
        }
    }

    /// Resolve `<short_name>` to a binary path. Falls back to looking up
    /// `dotagent-plugin-<short_name>` on `$PATH`.
    pub fn resolve(&self, short_name: &str) -> Result<PathBuf> {
        let binary_name = format!("dotagent-plugin-{short_name}");
        for dir in &self.search_paths {
            let candidate = dir.join(&binary_name);
            if candidate.is_file() {
                return Ok(candidate);
            }
        }
        // Final fallback: rely on $PATH resolution at spawn time.
        if which_on_path(&binary_name).is_some() {
            return Ok(PathBuf::from(binary_name));
        }
        Err(PluginError::NotFound(short_name.to_string()))
    }

    pub async fn info(&self, short_name: &str) -> Result<PluginInfo> {
        let bin = self.resolve(short_name)?;
        let output = Command::new(&bin).arg("info").output().await?;
        if !output.status.success() {
            return Err(PluginError::Failed {
                plugin: short_name.into(),
                code: output.status.code().unwrap_or(-1),
                stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
            });
        }
        Ok(serde_json::from_slice(&output.stdout)?)
    }

    pub async fn validate(
        &self,
        short_name: &str,
        config: &serde_json::Value,
    ) -> Result<PluginResponse> {
        self.run_with_stdin(short_name, "validate", config).await
    }

    pub async fn invoke(
        &self,
        short_name: &str,
        payload: &InvokePayload,
    ) -> Result<PluginResponse> {
        self.run_with_stdin(short_name, "invoke", payload).await
    }

    async fn run_with_stdin<T: Serialize>(
        &self,
        short_name: &str,
        verb: &str,
        payload: &T,
    ) -> Result<PluginResponse> {
        let bin = self.resolve(short_name)?;
        debug!(?bin, %verb, plugin = %short_name, "invoking plugin");

        let mut child = Command::new(&bin)
            .arg(verb)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;

        if let Some(mut stdin) = child.stdin.take() {
            let bytes = serde_json::to_vec(payload)?;
            stdin.write_all(&bytes).await?;
            stdin.shutdown().await?;
        }

        let output = child.wait_with_output().await?;
        if !output.status.success() {
            return Err(PluginError::Failed {
                plugin: short_name.into(),
                code: output.status.code().unwrap_or(-1),
                stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
            });
        }
        Ok(serde_json::from_slice(&output.stdout)?)
    }
}

fn which_on_path(binary: &str) -> Option<PathBuf> {
    std::env::var_os("PATH").and_then(|paths| {
        std::env::split_paths(&paths).find_map(|dir| {
            let candidate = dir.join(binary);
            if candidate.is_file() {
                Some(candidate)
            } else {
                None
            }
        })
    })
}
