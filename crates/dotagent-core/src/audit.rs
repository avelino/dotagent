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
    /// An inbound message was accepted and turned into a trigger.
    ///
    /// Records **who** and **where**, never the message text. Message bodies
    /// are user content and can carry secrets the operator pasted; the audit
    /// log is for attribution, not transcripts. Same posture as
    /// [`AuditEvent::SecretsLoaded`].
    TriggerReceived {
        source: String,
        /// Attested identity from the source (Telegram numeric user id).
        actor: String,
        /// Opaque conversation handle (Telegram chat id).
        reply_to: String,
    },
    /// An inbound message was refused before anything ran. Not-on-the-allowlist
    /// is the expected case, and it is `Critical` on purpose: someone found the
    /// bot.
    TriggerRejected {
        source: String,
        actor: String,
        reason: String,
    },
    /// An `agent.toml` exists but failed to parse or validate.
    ///
    /// `Critical` because the effect is invisible by nature: the agent simply
    /// never runs, and nothing else fires to tell you. Before this event, one
    /// typo could stop the whole scan and the only trace was a log line.
    ManifestInvalid {
        path: String,
        error: String,
    },
    /// A declared `[[preflight]] remediation` was executed.
    ///
    /// Distinct from `agent_run`: nothing in a manifest's `[run]` block ran,
    /// and the trigger was somebody in a chat window rather than the
    /// scheduler. `Critical` because it changes the machine on an inbound
    /// path — the one thing V8 exists to bound.
    RemediationInvoked {
        agent: String,
        plugin: String,
        /// The command as declared in the manifest, never anything a model
        /// composed.
        command: String,
        exit_code: i32,
        timed_out: bool,
    },
    /// A script packaged inside a skill was executed.
    ///
    /// Skills are text by default; `scripts/` makes one executable, and that
    /// is a distinct path into running code on this machine — it does not go
    /// through a manifest, so `AgentRun` never records it. Anyone auditing
    /// "what ran and why" needs this entry to see the whole picture.
    SkillInvoked {
        skill: String,
        /// Path relative to the skill directory. Absolute paths and traversal
        /// are refused before this event is written.
        script: String,
        exit_code: i32,
        timed_out: bool,
    },
    /// A `SKILL.md` exists but failed to parse or validate.
    ///
    /// Same reasoning as [`AuditEvent::ManifestInvalid`], one notch quieter:
    /// a broken skill removes a procedure from the catalog, which degrades an
    /// assistant's answers rather than stopping a scheduled run.
    SkillInvalid {
        path: String,
        error: String,
    },
    /// A run started from a trigger rather than a schedule window.
    AgentTriggered {
        source: String,
        actor: Option<String>,
        agent: String,
        schedule: String,
    },
    /// An inbound message named a command.
    ///
    /// The **name only**. Arguments are content, the same reason
    /// [`AuditEvent::TriggerReceived`] records who and where but never the
    /// message body — `/review ~/notes/salary.md` would otherwise put a path
    /// somebody typed into a chat onto disk forever.
    ///
    /// `known: false` is the interesting one: repeated unknown commands from an
    /// allowlisted sender is what probing looks like.
    CommandDispatched {
        command: String,
        actor: Option<String>,
        known: bool,
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
            | AuditEvent::TriggerReceived { .. }
            | AuditEvent::AgentTriggered { .. }
            | AuditEvent::SkillInvoked { .. }
            | AuditEvent::SkillInvalid { .. }
            // Notice even when unknown: a typo is the common cause, and
            // crying Critical over one would train the reader to skip them.
            // The signal is repetition, which a query over `known: false`
            // finds without the severity being wrong the rest of the time.
            | AuditEvent::CommandDispatched { .. }
            | AuditEvent::PluginInvoked { ok: false, .. } => Severity::Notice,

            AuditEvent::AgentRun { .. } /* non-zero exit */
            | AuditEvent::AgentGivenUp { .. }
            | AuditEvent::PreflightFailed { .. }
            | AuditEvent::ManifestDriftDetected { .. }
            | AuditEvent::PhantomAgentDetected { .. }
            | AuditEvent::SecretsRefused { .. }
            | AuditEvent::TriggerRejected { .. }
            | AuditEvent::ManifestInvalid { .. }
            | AuditEvent::RemediationInvoked { .. }
            | AuditEvent::AuditChainBroken { .. } => Severity::Critical,
        }
    }
}

pub const GENESIS_HASH: &str = "GENESIS";
