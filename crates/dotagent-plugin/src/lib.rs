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
use std::time::Duration;

use dotagent_supervisor::{ProcessKind, ProcessOwner, SpawnSpec, Supervisor, SupervisorError};
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
    #[error("plugin {plugin}.{verb} exceeded {deadline_seconds}s — killed")]
    TimedOut {
        plugin: String,
        verb: String,
        deadline_seconds: u64,
    },
    #[error("supervisor error: {0}")]
    Supervisor(#[from] SupervisorError),
}

/// Per-verb + per-kind deadlines enforced by the supervisor. Sensible
/// defaults; can be overridden via `PluginClient::with_timeouts` (e.g. when
/// the daemon reads a global config) or per-hook via the manifest's
/// `[[on_success]] timeout_seconds = …`.
#[derive(Debug, Clone)]
pub struct PluginTimeouts {
    pub info: Duration,
    pub validate: Duration,
    /// Generic invoke fallback — used when `invoke_with` is called without a
    /// resolvable `PluginKind` (today none of the call sites hit this, but
    /// keep it for forward-compat with future verbs).
    pub invoke: Duration,
    /// `[[preflight]]` invocations — short by design; preflight is a guard.
    pub preflight: Duration,
    /// `[[on_success]]` sinks — generous; sinks do real work (HTTP, file IO).
    pub sink: Duration,
    /// `[[on_failure]]` and notifier-via-plugin — fast, fire-and-forget.
    pub notify: Duration,
}

impl Default for PluginTimeouts {
    fn default() -> Self {
        Self {
            info: Duration::from_secs(10),
            validate: Duration::from_secs(30),
            invoke: Duration::from_secs(300),
            preflight: Duration::from_secs(30),
            sink: Duration::from_secs(300),
            notify: Duration::from_secs(15),
        }
    }
}

impl PluginTimeouts {
    /// Resolve the default deadline for a `PluginKind`. Used by `invoke_with`
    /// when the caller didn't pass an explicit override.
    pub fn for_kind(&self, kind: PluginKind) -> Duration {
        match kind {
            PluginKind::Preflight => self.preflight,
            PluginKind::Sink => self.sink,
            PluginKind::Notify => self.notify,
        }
    }
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

/// Resolves a plugin short name to a binary path and runs the protocol verbs
/// under the supervisor's deadline + kill-tree semantics.
#[derive(Clone)]
pub struct PluginClient {
    search_paths: Vec<PathBuf>,
    supervisor: Supervisor,
    timeouts: PluginTimeouts,
}

impl std::fmt::Debug for PluginClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PluginClient")
            .field("search_paths", &self.search_paths)
            .field("timeouts", &self.timeouts)
            .finish_non_exhaustive()
    }
}

impl PluginClient {
    /// Build the client from the standard discovery order with an in-process
    /// supervisor and default timeouts. The daemon should call
    /// `with_supervisor` to pass its singleton supervisor instead.
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
            supervisor: Supervisor::new(),
            timeouts: PluginTimeouts::default(),
        }
    }

    pub fn with_search_paths(paths: Vec<PathBuf>) -> Self {
        Self {
            search_paths: paths,
            supervisor: Supervisor::new(),
            timeouts: PluginTimeouts::default(),
        }
    }

    /// Use a shared supervisor (e.g. the daemon's singleton) instead of a
    /// per-client one. Required for `dotagent status`/`doctor` to see plugin
    /// processes spawned by hooks.
    #[must_use]
    pub fn with_supervisor(mut self, supervisor: Supervisor) -> Self {
        self.supervisor = supervisor;
        self
    }

    #[must_use]
    pub fn with_timeouts(mut self, timeouts: PluginTimeouts) -> Self {
        self.timeouts = timeouts;
        self
    }

    pub fn timeouts(&self) -> &PluginTimeouts {
        &self.timeouts
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
        let mut cmd = Command::new(&bin);
        cmd.arg("info");
        cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
        let spec = SpawnSpec {
            kind: ProcessKind::PluginInfo,
            owner: ProcessOwner {
                agent: "system".into(),
                plugin: Some(short_name.into()),
                ..Default::default()
            },
            deadline: self.timeouts.info,
            label: format!("{short_name}.info"),
        };
        let handle = self.supervisor.spawn_supervised(cmd, spec).await?;
        let output = match handle.wait_with_output().await {
            Ok(o) => o,
            Err(SupervisorError::TimedOut { deadline, .. }) => {
                return Err(PluginError::TimedOut {
                    plugin: short_name.into(),
                    verb: "info".into(),
                    deadline_seconds: deadline.as_secs(),
                });
            }
            Err(e) => return Err(e.into()),
        };
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
        let owner = ProcessOwner {
            agent: "system".into(),
            plugin: Some(short_name.into()),
            ..Default::default()
        };
        self.run_with_stdin(
            short_name,
            "validate",
            config,
            owner,
            ProcessKind::PluginValidate,
            self.timeouts.validate,
        )
        .await
    }

    /// Invoke the `invoke` verb with the default invoke timeout.
    pub async fn invoke(
        &self,
        short_name: &str,
        payload: &InvokePayload,
    ) -> Result<PluginResponse> {
        self.invoke_with(short_name, payload, None).await
    }

    /// Invoke the `invoke` verb with an optional per-call deadline override.
    /// Used by manifest hooks that declare `timeout_seconds` on the
    /// `[[on_success]]` / `[[preflight]]` entry.
    pub async fn invoke_with(
        &self,
        short_name: &str,
        payload: &InvokePayload,
        deadline_override: Option<Duration>,
    ) -> Result<PluginResponse> {
        let kind = match payload.kind {
            PluginKind::Notify => ProcessKind::Notify,
            PluginKind::Preflight => ProcessKind::Preflight,
            PluginKind::Sink => ProcessKind::Sink,
        };
        let owner = ProcessOwner {
            agent: payload.agent.clone(),
            schedule: Some(payload.schedule.clone()),
            hook_event: Some(payload.event.clone()),
            plugin: Some(short_name.into()),
        };
        self.run_with_stdin(
            short_name,
            "invoke",
            payload,
            owner,
            kind,
            deadline_override.unwrap_or_else(|| self.timeouts.for_kind(payload.kind)),
        )
        .await
    }

    async fn run_with_stdin<T: Serialize>(
        &self,
        short_name: &str,
        verb: &str,
        payload: &T,
        owner: ProcessOwner,
        kind: ProcessKind,
        deadline: Duration,
    ) -> Result<PluginResponse> {
        let bin = self.resolve(short_name)?;
        debug!(?bin, %verb, plugin = %short_name, deadline_secs = deadline.as_secs(), "invoking plugin");

        let mut cmd = Command::new(&bin);
        cmd.arg(verb)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        let spec = SpawnSpec {
            kind,
            owner,
            deadline,
            label: format!("{short_name}.{verb}"),
        };
        let mut handle = self.supervisor.spawn_supervised(cmd, spec).await?;

        if let Some(mut stdin) = handle.take_stdin() {
            let bytes = serde_json::to_vec(payload)?;
            stdin.write_all(&bytes).await?;
            stdin.shutdown().await?;
        }

        let output = match handle.wait_with_output().await {
            Ok(o) => o,
            Err(SupervisorError::TimedOut { deadline, .. }) => {
                return Err(PluginError::TimedOut {
                    plugin: short_name.into(),
                    verb: verb.into(),
                    deadline_seconds: deadline.as_secs(),
                });
            }
            Err(e) => return Err(e.into()),
        };
        if !output.status.success() {
            return Err(PluginError::Failed {
                plugin: short_name.into(),
                code: output.status.code().unwrap_or(-1),
                stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
            });
        }
        Ok(serde_json::from_slice(&output.stdout)?)
    }

    /// Borrow the underlying supervisor — used by `dotagent status`/`doctor`
    /// to enumerate live subprocesses regardless of who spawned them.
    pub fn supervisor(&self) -> &Supervisor {
        &self.supervisor
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
