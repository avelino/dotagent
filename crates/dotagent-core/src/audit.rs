//! Audit event types.
//!
//! Audit events form an append-only, hash-chained log at
//! `~/.local/share/dotagent/audit.log` (one JSON object per line).
//!
//! Each entry carries `prev_hash` (sha256 of the previous entry's full JSON
//! string) so tampering is detectable at startup: the daemon recomputes the
//! chain and emits `audit_chain_broken` (with notify) when it fails to
//! reconstruct.

use serde::{Deserialize, Serialize};

/// Severity of an audit event. Drives whether out-of-band notify fires.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    /// Routine — heartbeat, normal run start/end.
    Info,
    /// Worth keeping handy for forensics — non-critical change.
    Notice,
    /// Out-of-band notify. Includes given-up retries, drift, phantom agents.
    Critical,
}

/// A single audit log entry.
///
/// `prev_hash` chains entries together. The first entry has
/// `prev_hash = "GENESIS"`. The hash for any entry is sha256 of the entry's
/// canonical JSON serialization (with `prev_hash` set, computed over the
/// rest of the fields in declaration order).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuditEntry {
    pub ts: String,
    pub severity: Severity,
    pub event: AuditEvent,
    pub prev_hash: String,
}

/// All event kinds. Keep the enum exhaustive — unknown variants in the
/// log file mean the daemon downgraded; we want explicit cases.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "event_type", rename_all = "snake_case")]
pub enum AuditEvent {
    DaemonStarted {
        version: String,
        pid: u32,
    },
    DaemonStopped {
        reason: String,
    },
    AgentRun {
        agent: String,
        schedule: String,
        slug: String,
        manifest_sha256: String,
        exit_code: i32,
        duration_seconds: i64,
        timed_out: bool,
    },
    AgentRecovered {
        agent: String,
        schedule: String,
        attempts: u32,
    },
    AgentGivenUp {
        agent: String,
        schedule: String,
        attempts: u32,
        last_exit: i32,
        stderr_tail: String,
    },
    PreflightFailed {
        agent: String,
        schedule: String,
        plugin: String,
        suggest: Option<String>,
    },
    PluginInvoked {
        agent: String,
        plugin: String,
        plugin_kind: String,
        ok: bool,
    },
    ManifestLoaded {
        agent: String,
        path: String,
        sha256: String,
    },
    ManifestDriftDetected {
        agent: String,
        path: String,
        expected_sha256: String,
        actual_sha256: String,
    },
    PhantomAgentDetected {
        agent: String,
        path: String,
        sha256: String,
    },
    AuditChainBroken {
        position: usize,
        expected_prev_hash: String,
        actual_prev_hash: String,
    },
    TickStarted {
        agents_scanned: u32,
    },
    TickCompleted {
        agents_scanned: u32,
        runs_dispatched: u32,
        next_event_iso: Option<String>,
    },
    ConfigReloaded {
        reason: String,
    },
    /// Daemon loaded `~/.config/dotagent/secrets.env`. Records the path,
    /// resolved key count, and how many `op://` (or future scheme)
    /// references failed — **never** values. See
    /// `docs/concepts/secrets.md`.
    SecretsLoaded {
        path: String,
        key_count: usize,
        /// References (e.g., `op://...`) that the resolver could not
        /// expand. Those keys are unset in the store, on purpose, so
        /// notifier sends fail loud instead of leaking the literal
        /// `op://...` string.
        unresolved_references: usize,
    },
    /// Daemon refused to load the secrets file (insecure mode, parse
    /// error, IO error). Reason is human-readable, value-free.
    SecretsRefused {
        path: String,
        reason: String,
    },
}

impl AuditEvent {
    /// Default severity for an event kind. Callers can override if context
    /// demands (e.g., a recovered run after many attempts → still critical).
    pub fn default_severity(&self) -> Severity {
        match self {
            AuditEvent::DaemonStarted { .. }
            | AuditEvent::DaemonStopped { .. }
            | AuditEvent::TickStarted { .. }
            | AuditEvent::TickCompleted { .. }
            | AuditEvent::ManifestLoaded { .. }
            | AuditEvent::PluginInvoked { ok: true, .. } => Severity::Info,

            AuditEvent::AgentRun { exit_code: 0, .. }
            | AuditEvent::ConfigReloaded { .. }
            | AuditEvent::AgentRecovered { .. }
            | AuditEvent::SecretsLoaded { .. }
            | AuditEvent::PluginInvoked { ok: false, .. } => Severity::Notice,

            AuditEvent::AgentRun { .. } /* non-zero exit */
            | AuditEvent::AgentGivenUp { .. }
            | AuditEvent::PreflightFailed { .. }
            | AuditEvent::ManifestDriftDetected { .. }
            | AuditEvent::PhantomAgentDetected { .. }
            | AuditEvent::SecretsRefused { .. }
            | AuditEvent::AuditChainBroken { .. } => Severity::Critical,
        }
    }
}

pub const GENESIS_HASH: &str = "GENESIS";
