//! Agent manifest — the `agent.toml` file each agent declares.
//!
//! The manifest is the contract between the agent author and the orchestrator.
//! It declares how to run the agent, when to schedule it, what to do on
//! failure/success, and which preflight checks must pass first.
//!
//! The shape mirrors the legacy `meta.json` schema where possible so that the
//! migration path from the Fish-based agent-orchestrator is incremental.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};
use crate::security::SecurityConfig;

// Re-export so manifest authors can refer to `dotagent_core::NotifierEntry`.
pub use dotagent_notify::NotifierEntry;

/// Top-level manifest deserialised from `agent.toml`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentManifest {
    pub agent: AgentMeta,
    pub run: RunConfig,
    #[serde(default)]
    pub env: Option<EnvConfig>,
    #[serde(default)]
    pub defaults: ScheduleDefaults,
    #[serde(default, rename = "schedules")]
    pub schedules: Vec<Schedule>,
    #[serde(default)]
    pub preflight: Vec<PluginRef>,
    /// Built-in notifiers (`[[notifiers]]`). Native drivers run in-process
    /// — no plugin subprocess. The legacy `[[on_success]]` / `[[on_failure]]`
    /// arrays still work for sink/plugin escape hatches.
    #[serde(default)]
    pub notifiers: Vec<NotifierEntry>,
    #[serde(default)]
    pub on_success: Vec<PluginRef>,
    #[serde(default)]
    pub on_failure: Vec<PluginRef>,
    #[serde(default)]
    pub security: SecurityConfig,
}

/// Identity + meta-information about the agent.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentMeta {
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default = "default_monitor")]
    pub monitor: bool,
    #[serde(default = "default_timeout_seconds")]
    pub timeout_seconds: u64,
    #[serde(default)]
    pub version: Option<String>,
}

fn default_monitor() -> bool {
    true
}

fn default_timeout_seconds() -> u64 {
    1800
}

/// How to invoke the agent binary/script.
///
/// `command` is the executable, `args` is what it receives. The schedule's
/// `args` are appended at runtime.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunConfig {
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    /// Working directory, relative to the manifest directory. Default: `.`.
    #[serde(default)]
    pub working_dir: Option<PathBuf>,
}

/// Environment-variable injection rules.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct EnvConfig {
    #[serde(default = "default_inherit")]
    pub inherit: bool,
    #[serde(default)]
    pub extra: BTreeMap<String, String>,
}

fn default_inherit() -> bool {
    true
}

/// Agent-wide defaults applied to schedules that don't override them.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ScheduleDefaults {
    pub max_retries: Option<u32>,
    pub retry_backoff_minutes: Option<Vec<u32>>,
    pub stale_after_minutes: Option<u32>,
}

/// A schedule. Either cron-style (weekdays + hours + minute), interval-style
/// (every N minutes), or a free-form cron expression (future).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Schedule {
    Cron {
        id: String,
        /// 0=Sunday .. 6=Saturday (matches launchd Weekday).
        weekdays: Vec<u8>,
        hours: Vec<u8>,
        #[serde(default)]
        minute: u8,
        #[serde(default)]
        args: Vec<String>,
        #[serde(flatten)]
        overrides: ScheduleOverrides,
    },
    Interval {
        id: String,
        interval_minutes: u32,
        #[serde(default)]
        args: Vec<String>,
        #[serde(flatten)]
        overrides: ScheduleOverrides,
    },
    Expression {
        id: String,
        expression: String,
        #[serde(default)]
        args: Vec<String>,
        #[serde(flatten)]
        overrides: ScheduleOverrides,
    },
}

impl Schedule {
    pub fn id(&self) -> &str {
        match self {
            Schedule::Cron { id, .. }
            | Schedule::Interval { id, .. }
            | Schedule::Expression { id, .. } => id,
        }
    }

    pub fn args(&self) -> &[String] {
        match self {
            Schedule::Cron { args, .. }
            | Schedule::Interval { args, .. }
            | Schedule::Expression { args, .. } => args,
        }
    }

    pub fn overrides(&self) -> &ScheduleOverrides {
        match self {
            Schedule::Cron { overrides, .. }
            | Schedule::Interval { overrides, .. }
            | Schedule::Expression { overrides, .. } => overrides,
        }
    }
}

/// Per-schedule overrides that fall back to agent defaults when absent.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ScheduleOverrides {
    pub max_retries: Option<u32>,
    pub retry_backoff_minutes: Option<Vec<u32>>,
    pub stale_after_minutes: Option<u32>,
}

/// Reference to a plugin in `preflight` / `on_failure` / `on_success`.
///
/// `plugin` is the short name, resolved to a binary `dotagent-plugin-<name>` at
/// runtime via the plugin client. `config` is opaque JSON forwarded to the
/// plugin's `invoke` verb.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PluginRef {
    pub plugin: String,
    #[serde(default)]
    pub config: serde_json::Value,
    /// Optional event filter for `on_failure` / `on_success` (e.g.,
    /// `["given_up", "recovered"]`). Empty means "all events".
    #[serde(default)]
    pub events: Vec<String>,
}

impl AgentManifest {
    /// Load and parse an `agent.toml` from disk.
    pub fn load(path: impl AsRef<Path>) -> Result<Self> {
        let raw = std::fs::read_to_string(path.as_ref())?;
        let manifest: AgentManifest = toml::from_str(&raw)?;
        manifest.validate()?;
        Ok(manifest)
    }

    /// Run basic shape validation. Deeper validation (e.g., plugin existence)
    /// happens in the orchestrator's `doctor` command.
    pub fn validate(&self) -> Result<()> {
        if self.agent.name.is_empty() {
            return Err(Error::InvalidManifest("agent.name is empty".into()));
        }
        if self.run.command.is_empty() {
            return Err(Error::InvalidManifest("run.command is empty".into()));
        }
        let mut ids = std::collections::HashSet::new();
        for sched in &self.schedules {
            let id = sched.id();
            if !ids.insert(id.to_string()) {
                return Err(Error::InvalidManifest(format!(
                    "duplicate schedule id: {id}"
                )));
            }
        }
        Ok(())
    }
}
