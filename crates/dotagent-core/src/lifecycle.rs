//! `[lifecycle]` section of the manifest — how long one run lives.
//!
//! Every agent is a one-shot subprocess by default: dotagent spawns it, waits
//! for it, and reaps it. That is the right shape for a cron agent and the
//! wrong one for a dispatcher, which pays for process startup and for
//! reloading whatever state it holds on every single message.
//!
//! `mode = "persistent"` keeps the process alive between runs and delivers
//! requests to it over JSON lines. The daemon owns its lifecycle the same way
//! it owns every other subprocess: same supervisor, same deadline, same
//! kill-tree, same `dotagent status`.
//!
//! `oneshot` stays the default forever — an agent with no `[lifecycle]`
//! behaves exactly as it did before this section existed. See
//! `docs/concepts/lifecycle.md` and `docs/reference/persistent-protocol.md`.

use serde::{Deserialize, Serialize};

/// How long one agent process lives.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LifecycleMode {
    /// Spawn, run, exit. One process per event.
    #[default]
    Oneshot,
    /// Stay alive between events, receiving requests over JSON lines.
    Persistent,
}

/// Default idle window before a persistent instance is recycled.
pub const DEFAULT_IDLE_TIMEOUT_SECONDS: u64 = 1800;
/// Default number of requests one instance answers before being recycled.
pub const DEFAULT_MAX_INVOCATIONS: u32 = 120;
/// Default window for a freshly spawned instance to answer the handshake.
pub const DEFAULT_STARTUP_TIMEOUT_SECONDS: u64 = 30;
/// Default ceiling on live instances of one persistent agent.
pub const DEFAULT_MAX_INSTANCES: u32 = 8;

/// Per-agent process lifecycle declaration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LifecycleConfig {
    #[serde(default)]
    pub mode: LifecycleMode,

    /// Recycle an instance that has answered nothing for this long. A process
    /// holding a conversation is not free, and one nobody is talking to is
    /// pure cost.
    #[serde(default = "default_idle_timeout_seconds")]
    pub idle_timeout_seconds: u64,

    /// Recycle an instance after this many requests. `0` means never — which
    /// is a real choice for an agent whose state does not grow, and a bad one
    /// for anything holding a transcript.
    #[serde(default = "default_max_invocations")]
    pub max_invocations: u32,

    /// How long a newly spawned instance has to answer the `hello` handshake.
    #[serde(default = "default_startup_timeout_seconds")]
    pub startup_timeout_seconds: u64,

    /// Field of the trigger payload that decides *which* instance answers.
    ///
    /// A dispatcher handling several conversations wants one process each:
    /// with a single instance, everything anyone says lands in the same
    /// process, and so does whatever state it kept. `key = "chat_id"` gives
    /// one instance per conversation.
    ///
    /// Absent means a single instance for the whole agent, which is right for
    /// a warm cache and wrong for anything holding per-sender context.
    #[serde(default)]
    pub key: Option<String>,

    /// Ceiling on live instances. Past it, the least recently used one is
    /// terminated to make room.
    #[serde(default = "default_max_instances")]
    pub max_instances: u32,
}

fn default_idle_timeout_seconds() -> u64 {
    DEFAULT_IDLE_TIMEOUT_SECONDS
}

fn default_max_invocations() -> u32 {
    DEFAULT_MAX_INVOCATIONS
}

fn default_startup_timeout_seconds() -> u64 {
    DEFAULT_STARTUP_TIMEOUT_SECONDS
}

fn default_max_instances() -> u32 {
    DEFAULT_MAX_INSTANCES
}

impl Default for LifecycleConfig {
    fn default() -> Self {
        Self {
            mode: LifecycleMode::Oneshot,
            idle_timeout_seconds: DEFAULT_IDLE_TIMEOUT_SECONDS,
            max_invocations: DEFAULT_MAX_INVOCATIONS,
            startup_timeout_seconds: DEFAULT_STARTUP_TIMEOUT_SECONDS,
            key: None,
            max_instances: DEFAULT_MAX_INSTANCES,
        }
    }
}

impl LifecycleConfig {
    pub fn is_persistent(&self) -> bool {
        matches!(self.mode, LifecycleMode::Persistent)
    }

    /// Shape validation. Called from `AgentManifest::validate`, which knows
    /// `[agent].timeout_seconds` and passes it in — a startup window longer
    /// than the request deadline means the first message can never succeed.
    pub fn validate(&self, agent_timeout_seconds: u64) -> Result<(), String> {
        if !self.is_persistent() {
            return Ok(());
        }
        if self.idle_timeout_seconds == 0 {
            return Err("lifecycle.idle_timeout_seconds must be > 0".into());
        }
        if self.max_instances == 0 {
            return Err("lifecycle.max_instances must be >= 1".into());
        }
        if self.startup_timeout_seconds == 0 {
            return Err("lifecycle.startup_timeout_seconds must be > 0".into());
        }
        if self.startup_timeout_seconds >= agent_timeout_seconds {
            return Err(format!(
                "lifecycle.startup_timeout_seconds ({}) must be below agent.timeout_seconds ({})",
                self.startup_timeout_seconds, agent_timeout_seconds
            ));
        }
        if let Some(key) = &self.key {
            if key.trim().is_empty() {
                return Err("lifecycle.key is empty — omit it for a single instance".into());
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn absent_section_is_oneshot_with_defaults() {
        let cfg = LifecycleConfig::default();
        assert!(!cfg.is_persistent());
        assert_eq!(cfg.idle_timeout_seconds, DEFAULT_IDLE_TIMEOUT_SECONDS);
        assert_eq!(cfg.max_invocations, DEFAULT_MAX_INVOCATIONS);
        assert_eq!(cfg.max_instances, DEFAULT_MAX_INSTANCES);
        assert!(cfg.key.is_none());
    }

    #[test]
    fn a_oneshot_agent_is_never_rejected_for_persistent_only_rules() {
        // Nonsense values still parse: they simply do not apply.
        let cfg = LifecycleConfig {
            idle_timeout_seconds: 0,
            max_instances: 0,
            startup_timeout_seconds: 0,
            ..Default::default()
        };
        assert!(cfg.validate(1).is_ok());
    }

    #[test]
    fn persistent_rejects_a_startup_window_past_the_request_deadline() {
        let cfg = LifecycleConfig {
            mode: LifecycleMode::Persistent,
            startup_timeout_seconds: 600,
            ..Default::default()
        };
        let err = cfg.validate(600).unwrap_err();
        assert!(err.contains("startup_timeout_seconds"), "{err}");
    }

    #[test]
    fn persistent_rejects_zero_idle_and_zero_instances() {
        let base = LifecycleConfig {
            mode: LifecycleMode::Persistent,
            ..Default::default()
        };
        assert!(LifecycleConfig {
            idle_timeout_seconds: 0,
            ..base.clone()
        }
        .validate(1800)
        .is_err());
        assert!(LifecycleConfig {
            max_instances: 0,
            ..base.clone()
        }
        .validate(1800)
        .is_err());
        assert!(LifecycleConfig {
            key: Some("  ".into()),
            ..base
        }
        .validate(1800)
        .is_err());
    }

    #[test]
    fn max_invocations_zero_means_unlimited_and_is_legal() {
        let cfg = LifecycleConfig {
            mode: LifecycleMode::Persistent,
            max_invocations: 0,
            ..Default::default()
        };
        assert!(cfg.validate(1800).is_ok());
    }

    #[test]
    fn mode_round_trips_as_snake_case() {
        let toml = r#"mode = "persistent""#;
        let cfg: LifecycleConfig = toml::from_str(toml).unwrap();
        assert!(cfg.is_persistent());
        // Defaults fill the rest — declaring the mode is enough to opt in.
        assert_eq!(cfg.idle_timeout_seconds, DEFAULT_IDLE_TIMEOUT_SECONDS);
    }
}
