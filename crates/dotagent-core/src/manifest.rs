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

use crate::assistant::ASSISTANT_PROTOCOL_V1;
use crate::error::{Error, Result};
use crate::lifecycle::LifecycleConfig;
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
    /// How long one process lives. Absent = `oneshot`, the shape every agent
    /// had before this section existed.
    #[serde(default)]
    pub lifecycle: LifecycleConfig,
    /// Conversational-assistant harness opt-in. Absent = plain runs, byte
    /// for byte what every agent did before this section existed.
    #[serde(default)]
    pub assistant: Option<AssistantConfig>,
    /// Long-term memory for a plain (non-assistant) agent. Absent = the
    /// agent's output is never read for facts.
    #[serde(default)]
    pub memory: Option<AgentMemoryConfig>,
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
    /// Stdout protocol the agent speaks. `None` ⇒ plain run: stdout is
    /// captured as the run's output tail. `assistant-v1` ⇒ each stdout line
    /// is an [`crate::assistant::AssistantEvent`] streamed back to the client
    /// that triggered the run.
    #[serde(default)]
    pub protocol: Option<String>,
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

/// Opt-in conversational-assistant harness (`[assistant]`).
///
/// Present = the daemon keeps conversation pointers for this agent's
/// triggered runs (model session id, toolkit hash, transcript size),
/// reinjects them on the next trigger, and captures `MEMO:` lines from
/// replies into the memory workspace. Absent = none of that happens.
///
/// Pointers, never transcripts: the daemon records which model session
/// served a conversation, never what was said.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AssistantConfig {
    /// Master switch inside the section — disable the harness without
    /// deleting the block.
    #[serde(default = "default_assistant_enabled")]
    pub enabled: bool,
    /// Recall stored facts before each run and capture `MEMO:` lines from
    /// replies into the memory workspace.
    #[serde(default = "default_assistant_memory")]
    pub memory: bool,
    /// Transcript retirement ceiling in bytes. `None` = the daemon default
    /// (`dotagent-assistant::registry::DEFAULT_TRANSCRIPT_BYTES_MAX`).
    #[serde(default)]
    pub transcript_bytes_max: Option<u64>,
    #[serde(default)]
    pub toolkit: AssistantToolkit,
}

fn default_assistant_enabled() -> bool {
    true
}

fn default_assistant_memory() -> bool {
    true
}

/// Opt-in memory capture for an ordinary agent (`[memory]`).
///
/// The assistant harness already reads `MEMO:` lines out of a reply. This is
/// the same capture for every other agent: a scheduled run that learns
/// something durable prints `MEMO: <fact> | topics: a, b` and the daemon
/// files it, with the agent's name recorded as provenance.
///
/// Off unless declared, because most agents are not writing facts — they are
/// printing status, and a store that absorbs status is a store whose recall
/// returns status.
///
/// ```toml
/// [memory]
/// capture = true
/// topics = ["ops"]   # added to every fact this agent files
/// ```
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentMemoryConfig {
    /// Scan this agent's stdout for `MEMO:` lines.
    #[serde(default = "default_capture")]
    pub capture: bool,
    /// Topics added to every fact this agent files, on top of whatever the
    /// `MEMO:` line named. A cost agent tagging everything `ops` means its
    /// facts stay findable as a group without the agent restating it on
    /// every line.
    #[serde(default)]
    pub topics: Vec<String>,
}

fn default_capture() -> bool {
    true
}

impl Default for AgentMemoryConfig {
    fn default() -> Self {
        Self {
            capture: true,
            topics: Vec::new(),
        }
    }
}

impl AgentMemoryConfig {
    /// Whether the daemon should read this agent's output for facts.
    pub fn captures(&self) -> bool {
        self.capture
    }
}

/// The MCP servers a conversation's model client runs with.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AssistantToolkit {
    /// Empty = the agent provisions its own toolkit (compatibility with
    /// agents that predate the harness).
    #[serde(default)]
    pub servers: Vec<ToolkitServer>,
}

/// One MCP server in an `[assistant]` toolkit.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ToolkitServer {
    /// The daemon's own MCP server (`dotagent mcp`).
    Dotagent,
    /// An external HTTP MCP endpoint (e.g. the local proxy).
    Http { url: String },
    /// An external stdio MCP server.
    Stdio { command: String, args: Vec<String> },
}

impl ToolkitServer {
    /// The server's key inside the assembled `mcp.json`. Doubles as the
    /// sort key for byte-stable assembly and as the duplicate-detection
    /// key at validation time.
    pub fn name(&self) -> &str {
        match self {
            ToolkitServer::Dotagent => "dotagent",
            ToolkitServer::Http { .. } => "mcp",
            ToolkitServer::Stdio { .. } => "stdio",
        }
    }
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
    /// Overrides `[power] on_battery` from `config.toml` for this schedule.
    ///
    /// Per-schedule rather than per-agent because the cost is per-schedule: an
    /// agent can reasonably keep its cheap hourly check on battery while its
    /// expensive every-15-minutes sync waits for a charger.
    pub on_battery: Option<crate::power::PowerPolicy>,
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
    /// What to run to clear this check, when it can be cleared by running
    /// something.
    ///
    /// A preflight plugin already returns a `suggest` string, and the obvious
    /// next step is to let an assistant run it. That step is the one not
    /// taken: `suggest` is written by the plugin, and executing a string a
    /// plugin chose, triggered by a chat message, is arbitrary execution from
    /// an inbound path (V8 in the threat model).
    ///
    /// This is the operator saying it instead. Declared here, it becomes a
    /// named entry in the MCP catalog — so a model *picks* it rather than
    /// composing it, which is the same property `tools/list` gives agents.
    ///
    /// ```toml
    /// [[preflight]]
    /// plugin = "preflight-warp"
    /// remediation = "warp-cli connect"
    /// ```
    ///
    /// Split on whitespace into argv and executed directly. There is no shell,
    /// so pipes, `&&` and globs are literal arguments rather than syntax.
    #[serde(default)]
    pub remediation: Option<String>,
    /// Optional per-hook deadline override (seconds). When set, the
    /// supervisor uses it instead of the global default for the plugin
    /// client's `invoke` verb. Useful for hooks that legitimately need
    /// more time (e.g. a sink writing many blocks to a slow upstream API).
    #[serde(default)]
    pub timeout_seconds: Option<u64>,
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
        // An unknown protocol must fail at load, not mid-run: the daemon would
        // otherwise try to parse a stream it has no reader for and hand the
        // client silence.
        if let Some(protocol) = &self.run.protocol {
            if protocol != ASSISTANT_PROTOCOL_V1 {
                return Err(Error::InvalidManifest(format!(
                    "run.protocol: unsupported protocol {protocol:?} (supported: {ASSISTANT_PROTOCOL_V1})"
                )));
            }
        }
        if self.run.protocol.as_deref() == Some(ASSISTANT_PROTOCOL_V1)
            && self.lifecycle.is_persistent()
        {
            return Err(Error::InvalidManifest(
                "run.protocol = \"assistant-v1\" is incompatible with lifecycle.mode = \"persistent\"; use the persistent JSON-lines protocol instead".into(),
            ));
        }
        if let Some(assistant) = &self.assistant {
            // A zero ceiling would retire the session on every frame — the
            // harness would amnesia-loop the conversation.
            if assistant.transcript_bytes_max == Some(0) {
                return Err(Error::InvalidManifest(
                    "assistant.transcript_bytes_max must be greater than 0".into(),
                ));
            }
            // Two servers under the same mcp.json key cannot coexist; the
            // JSON object would silently keep only the last one.
            let mut names = std::collections::HashSet::new();
            for server in &assistant.toolkit.servers {
                if !names.insert(server.name()) {
                    return Err(Error::InvalidManifest(format!(
                        "assistant.toolkit.servers: duplicate MCP server name '{}'",
                        server.name()
                    )));
                }
            }
        }
        let mut ids = std::collections::HashSet::new();
        for sched in &self.schedules {
            let id = sched.id();
            if !ids.insert(id.to_string()) {
                return Err(Error::InvalidManifest(format!(
                    "duplicate schedule id: {id}"
                )));
            }
            // A zero interval has no cadence, and the scheduler cannot invent
            // one. It survives every gate as "already succeeded", so the agent
            // reads `ok` forever and is never dispatched — a silent death, from
            // a typo, in the one subsystem whose job is to make death loud.
            if matches!(
                sched,
                Schedule::Interval {
                    interval_minutes: 0,
                    ..
                }
            ) {
                return Err(Error::InvalidManifest(format!(
                    "schedule {id}: interval_minutes must be greater than 0"
                )));
            }
        }
        self.lifecycle
            .validate(self.agent.timeout_seconds)
            .map_err(Error::InvalidManifest)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lifecycle::{LifecycleMode, DEFAULT_IDLE_TIMEOUT_SECONDS};

    fn parse(extra: &str) -> Result<AgentManifest> {
        let raw = format!(
            r#"
            [agent]
            name = "x"
            [run]
            command = "bash"
            {extra}
            "#
        );
        let manifest: AgentManifest = toml::from_str(&raw).map_err(Error::from)?;
        manifest.validate()?;
        Ok(manifest)
    }

    #[test]
    fn a_manifest_without_lifecycle_is_oneshot() {
        let m = parse("").unwrap();
        assert_eq!(m.lifecycle.mode, LifecycleMode::Oneshot);
        assert!(!m.lifecycle.is_persistent());
    }

    #[test]
    fn declaring_the_mode_is_enough_to_opt_in() {
        let m = parse("[lifecycle]\nmode = \"persistent\"").unwrap();
        assert!(m.lifecycle.is_persistent());
        assert_eq!(
            m.lifecycle.idle_timeout_seconds,
            DEFAULT_IDLE_TIMEOUT_SECONDS
        );
    }

    #[test]
    fn lifecycle_fields_override_the_defaults() {
        let m = parse(
            r#"[lifecycle]
            mode = "persistent"
            idle_timeout_seconds = 60
            max_invocations = 5
            startup_timeout_seconds = 10
            key = "chat_id"
            max_instances = 2"#,
        )
        .unwrap();
        assert_eq!(m.lifecycle.idle_timeout_seconds, 60);
        assert_eq!(m.lifecycle.max_invocations, 5);
        assert_eq!(m.lifecycle.startup_timeout_seconds, 10);
        assert_eq!(m.lifecycle.key.as_deref(), Some("chat_id"));
        assert_eq!(m.lifecycle.max_instances, 2);
    }

    #[test]
    fn an_impossible_startup_window_fails_the_manifest() {
        // agent.timeout_seconds defaults to 1800; a longer handshake window
        // means the first message can never land.
        let err = parse("[lifecycle]\nmode = \"persistent\"\nstartup_timeout_seconds = 3600")
            .unwrap_err();
        assert!(err.to_string().contains("startup_timeout_seconds"), "{err}");
    }

    #[test]
    fn an_unknown_mode_is_a_parse_error_not_a_silent_oneshot() {
        assert!(parse("[lifecycle]\nmode = \"forever\"").is_err());
    }

    /// `interval_minutes = 0` used to load fine and then never dispatch: the
    /// scheduler treats it as permanently up to date, so the agent reports
    /// `ok` and never runs again. Rejecting it at load turns a silent death
    /// into a startup error.
    #[test]
    fn a_zero_interval_is_rejected_instead_of_never_dispatching() {
        let err = parse("[[schedules]]\nid = \"q\"\ntype = \"interval\"\ninterval_minutes = 0")
            .unwrap_err();
        assert!(err.to_string().contains("interval_minutes"), "{err}");
    }

    #[test]
    fn a_positive_interval_still_loads() {
        let m =
            parse("[[schedules]]\nid = \"q\"\ntype = \"interval\"\ninterval_minutes = 90").unwrap();
        assert_eq!(m.schedules.len(), 1);
    }

    #[test]
    fn the_assistant_protocol_is_accepted() {
        let m = parse("protocol = \"assistant-v1\"").unwrap();
        assert!(!m.lifecycle.is_persistent());
        assert_eq!(
            m.run.protocol.as_deref(),
            Some(crate::assistant::ASSISTANT_PROTOCOL_V1)
        );
    }

    #[test]
    fn the_assistant_protocol_is_rejected_for_persistent_lifecycle() {
        let err =
            parse("protocol = \"assistant-v1\"\n[lifecycle]\nmode = \"persistent\"").unwrap_err();
        assert_eq!(
            err.to_string(),
            "invalid manifest: run.protocol = \"assistant-v1\" is incompatible with lifecycle.mode = \"persistent\"; use the persistent JSON-lines protocol instead"
        );
    }

    #[test]
    fn an_unknown_protocol_fails_fast_at_load() {
        let err = parse("protocol = \"assistant-v2\"").unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("assistant-v2"), "{msg}");
        assert!(msg.contains("assistant-v1"), "{msg}");
    }

    #[test]
    fn no_protocol_declared_stays_none() {
        let m = parse("").unwrap();
        assert!(m.run.protocol.is_none());
    }

    #[test]
    fn a_manifest_without_memory_section_stays_none() {
        // Most agents print status, not facts. Reading their output for
        // memories by default would fill the store with status.
        assert!(parse("").unwrap().memory.is_none());
    }

    #[test]
    fn a_bare_memory_section_captures() {
        let m = parse("[memory]").unwrap().memory.expect("section present");
        assert!(m.capture);
        assert!(m.topics.is_empty());
    }

    #[test]
    fn memory_capture_can_be_disabled_while_keeping_the_section() {
        let m = parse("[memory]\ncapture = false\ntopics = [\"ops\"]")
            .unwrap()
            .memory
            .expect("section present");
        assert!(!m.capture);
        assert_eq!(m.topics, vec!["ops".to_string()]);
    }

    #[test]
    fn a_manifest_without_assistant_section_stays_none() {
        let m = parse("").unwrap();
        assert!(m.assistant.is_none());
    }

    #[test]
    fn a_minimal_assistant_section_gets_defaults() {
        let m = parse("[assistant]").unwrap();
        let a = m.assistant.expect("section present");
        assert!(a.enabled);
        assert!(a.memory);
        assert_eq!(a.transcript_bytes_max, None);
        assert!(a.toolkit.servers.is_empty());
    }

    #[test]
    fn an_explicit_assistant_section_parses() {
        let m = parse(
            "[assistant]\nenabled = false\nmemory = false\ntranscript_bytes_max = 1024\n\
             [[assistant.toolkit.servers]]\nkind = \"dotagent\"\n\
             [[assistant.toolkit.servers]]\nkind = \"http\"\nurl = \"http://127.0.0.1:7333/mcp\"\n",
        )
        .unwrap();
        let a = m.assistant.expect("section present");
        assert!(!a.enabled);
        assert!(!a.memory);
        assert_eq!(a.transcript_bytes_max, Some(1024));
        assert_eq!(a.toolkit.servers.len(), 2);
        assert_eq!(a.toolkit.servers[0], ToolkitServer::Dotagent);
        assert_eq!(
            a.toolkit.servers[1],
            ToolkitServer::Http {
                url: "http://127.0.0.1:7333/mcp".into()
            }
        );
    }

    #[test]
    fn stdio_toolkit_server_carries_command_and_args() {
        let m = parse(
            "[[assistant.toolkit.servers]]\nkind = \"stdio\"\ncommand = \"mcp\"\nargs = [\"serve\"]\n",
        )
        .unwrap();
        let a = m.assistant.expect("section present");
        assert_eq!(
            a.toolkit.servers[0],
            ToolkitServer::Stdio {
                command: "mcp".into(),
                args: vec!["serve".into()],
            }
        );
    }

    #[test]
    fn duplicate_toolkit_server_names_are_rejected() {
        let err = parse(
            "[[assistant.toolkit.servers]]\nkind = \"http\"\nurl = \"http://a/mcp\"\n\
             [[assistant.toolkit.servers]]\nkind = \"http\"\nurl = \"http://b/mcp\"\n",
        )
        .unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("duplicate MCP server name 'mcp'"), "{msg}");
    }

    #[test]
    fn zero_transcript_ceiling_is_rejected() {
        let err = parse("[assistant]\ntranscript_bytes_max = 0\n").unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("transcript_bytes_max"), "{msg}");
    }
}
