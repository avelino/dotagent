//! Core types for dotagent.
//!
//! This crate is the schema contract — all other crates depend on the types
//! defined here. Keeping it tiny and IO-free is intentional: deserialization,
//! validation, and shape-of-data lives here, side effects live elsewhere.

pub mod audit;
pub mod config;
pub mod error;
pub mod heartbeat;
pub mod manifest;
pub mod security;
pub mod state;

pub use audit::{AuditEntry, AuditEvent, Severity, GENESIS_HASH};
pub use config::{Config, LoggingConfig, SecretsConfig, TelemetryConfig};
pub use error::{Error, Result};
pub use heartbeat::Heartbeat;
pub use manifest::{
    AgentManifest, AgentMeta, EnvConfig, PluginRef, RunConfig, Schedule, ScheduleDefaults,
};
pub use security::{NetworkMode, NetworkPolicy, SecurityConfig};
pub use state::WindowState;
