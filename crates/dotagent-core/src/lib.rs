//! Core types for dotagent.
//!
//! This crate is the schema contract — all other crates depend on the types
//! defined here. Keeping it tiny and IO-free is intentional: deserialization,
//! validation, and shape-of-data lives here, side effects live elsewhere.

pub mod audit;
pub mod command;
pub mod config;
pub mod error;
pub mod frontmatter;
pub mod heartbeat;
pub mod lifecycle;
pub mod manifest;
pub mod security;
pub mod skill;
pub mod state;
pub mod trigger;

pub use audit::{AuditEntry, AuditEvent, Severity, GENESIS_HASH};
pub use command::{CommandManifest, COMMAND_EXT, NAMESPACE_SEP};
pub use config::{
    CommandsConfig, Config, LoggingConfig, SecretsConfig, SkillsConfig, TelegramIngressConfig,
    TelemetryConfig,
};
pub use error::{Error, Result};
pub use heartbeat::Heartbeat;
pub use lifecycle::{LifecycleConfig, LifecycleMode};
pub use manifest::{
    AgentManifest, AgentMeta, EnvConfig, PluginRef, RunConfig, Schedule, ScheduleDefaults,
};
pub use security::{NetworkMode, NetworkPolicy, SecurityConfig};
pub use skill::{SkillManifest, SKILL_FILE};
pub use state::WindowState;
pub use trigger::{TriggerRequest, TriggerSource, TRIGGER_SCHEDULE_ID};
