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
    /// A process a previous daemon left running was killed at boot.
    ///
    /// Identity is confirmed before any signal — group leadership, start time
    /// and command all have to match the snapshot, so a recycled pid is
    /// refused rather than killed. See `dotagent_supervisor::orphan`.
    OrphanReaped {
        agent: String,
        kind: String,
        label: String,
        pid: u32,
        age_seconds: u64,
        deadline_seconds: u64,
    },
    AuditChainBroken {
        position: usize,
        expected_prev_hash: String,
        actual_prev_hash: String,
    },
    /// The log was rotated: everything up to `tail_hash` now lives in
    /// `rotated_to`, and this entry is the first line of the fresh file.
    ///
    /// It is the **seam**. Its `prev_hash` equals `tail_hash`, so the chain
    /// continues across the rename with no gap — and its presence is what
    /// lets verification tell retention from truncation. An operator who
    /// deletes an old segment leaves this entry behind, still covered by the
    /// chain, declaring exactly what went away; someone who cuts lines off the
    /// head of the current file removes the seam along with them, and the
    /// orphaned `prev_hash` that remains is accounted for by nothing.
    ///
    /// That distinction is the whole reason a rotated audit log is still an
    /// audit log. See `docs/security/threat-model.md` for what it does and
    /// does not buy.
    AuditLogRotated {
        /// File name only, never a path — the segment is a sibling of
        /// `audit.log`. Verification refuses anything containing a separator,
        /// because this string comes off disk like any other attacker-writable
        /// field.
        rotated_to: String,
        /// How many entries moved. Recorded here so a segment that is gone can
        /// still be described.
        entries: usize,
        /// `ts` of the first entry in the rotated segment, or `"unknown"` when
        /// its head was already unreadable.
        first_ts: String,
        /// sha256 of the segment's last line. Equals this entry's `prev_hash`;
        /// a seam where the two disagree is not a seam.
        tail_hash: String,
    },
    /// **Read-only.** No longer emitted — see [`AuditEvent::TickCompleted`].
    TickStarted {
        agents_scanned: u32,
    },
    /// **Read-only.** No longer emitted.
    ///
    /// A tick is telemetry, not an auditable event: "woke up and looked at 17
    /// agents" tells a forensic reader nothing, and it dominated the file —
    /// 64% of one real 38k-entry log was these two variants. Because
    /// `AuditLog::append` re-reads the whole file to find the tail hash, the
    /// noise also made every event worth recording quadratically more
    /// expensive to write. The daemon emits `tracing` at `debug` instead, which
    /// already rotates.
    ///
    /// **Kept so old logs stay readable.** Deleting the variants would make
    /// `dotagent audit verify` and `verify_chain` fail on every existing
    /// install the moment they reach a historic tick entry — and a hash chain
    /// you can no longer verify is worse than one carrying dead weight.
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
    /// A persistent agent instance was spawned and answered the handshake.
    ///
    /// One `AgentRun` per message still gets written; this records the thing
    /// that outlives them — which process is holding state for which key, and
    /// since when.
    PersistentAgentStarted {
        agent: String,
        /// The `[lifecycle] key` value this instance answers for, or
        /// `"default"` when the agent declared no key.
        key: String,
        pid: u32,
    },
    /// A persistent agent instance went away.
    ///
    /// `reason` is why, and it is the whole point of the event: an instance
    /// that died because a conversation grew past `max_invocations` is normal
    /// housekeeping, and one that died because it crashed is a bug somebody
    /// should see.
    PersistentAgentRecycled {
        agent: String,
        key: String,
        /// `idle` | `max_invocations` | `crashed` | `timeout` | `evicted`
        /// | `shutdown` | `config_changed`
        reason: String,
        /// How many requests it answered before going away.
        invocations: u32,
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
            | AuditEvent::PersistentAgentStarted { .. }
            // Not Info: rotation restructures the forensic artifact, and a
            // reader who missed it would mistake the seam for a truncated log.
            // Not Critical: it is the log doing its housekeeping on schedule.
            | AuditEvent::AuditLogRotated { .. }
            | AuditEvent::PluginInvoked { ok: false, .. } => Severity::Notice,

            // Recycling is routine except when it isn't. `crashed` and
            // `timeout` mean the process failed to hold up its end; the rest
            // are the pool doing exactly what the manifest asked for.
            AuditEvent::PersistentAgentRecycled { reason, .. } => {
                match reason.as_str() {
                    "crashed" | "timeout" => Severity::Critical,
                    _ => Severity::Notice,
                }
            }

            AuditEvent::AgentRun { .. } /* non-zero exit */
            | AuditEvent::AgentGivenUp { .. }
            | AuditEvent::PreflightFailed { .. }
            | AuditEvent::ManifestDriftDetected { .. }
            | AuditEvent::PhantomAgentDetected { .. }
            | AuditEvent::OrphanReaped { .. }
            | AuditEvent::SecretsRefused { .. }
            | AuditEvent::TriggerRejected { .. }
            | AuditEvent::ManifestInvalid { .. }
            | AuditEvent::RemediationInvoked { .. }
            | AuditEvent::AuditChainBroken { .. } => Severity::Critical,
        }
    }
}

pub const GENESIS_HASH: &str = "GENESIS";

#[cfg(test)]
mod tests {
    use super::*;

    /// Verbatim lines from a real `audit.log`, written before the daemon
    /// stopped emitting ticks. Tens of thousands of these exist on installs in
    /// the wild; `dotagent audit verify` and `verify_chain` walk every one of
    /// them, so the day they stop parsing is the day the chain stops being
    /// verifiable.
    const HISTORIC_TICK_STARTED: &str = r#"{"ts":"2026-05-19T08:31:07-0300","severity":"info","event":{"event_type":"tick_started","agents_scanned":0},"prev_hash":"2c94629858b453eef01ea41eca0783054a3b6816adb2b9997e0bdbfce2cb9698"}"#;
    const HISTORIC_TICK_COMPLETED: &str = r#"{"ts":"2026-05-19T08:31:07-0300","severity":"info","event":{"event_type":"tick_completed","agents_scanned":17,"runs_dispatched":2,"next_event_iso":"2026-05-19T09:00:00-0300"},"prev_hash":"a69720c4ed3b953b0507257af6cca66e9131d52cc97305de489d99b0116b2801"}"#;

    #[test]
    fn historic_tick_entries_still_deserialize() {
        let started: AuditEntry = serde_json::from_str(HISTORIC_TICK_STARTED).unwrap();
        assert!(matches!(
            started.event,
            AuditEvent::TickStarted { agents_scanned: 0 }
        ));

        let completed: AuditEntry = serde_json::from_str(HISTORIC_TICK_COMPLETED).unwrap();
        match completed.event {
            AuditEvent::TickCompleted {
                agents_scanned,
                runs_dispatched,
                next_event_iso,
            } => {
                assert_eq!((agents_scanned, runs_dispatched), (17, 2));
                assert_eq!(next_event_iso.as_deref(), Some("2026-05-19T09:00:00-0300"));
            }
            other => panic!("expected tick_completed, got {other:?}"),
        }
    }

    #[test]
    fn historic_tick_entries_re_serialize_byte_for_byte() {
        // `verify_chain` recomputes each hash from the entry's JSON. A field
        // reordered or renamed would leave every historic hash unreproducible
        // and report the chain as broken — indistinguishable from tampering.
        for line in [HISTORIC_TICK_STARTED, HISTORIC_TICK_COMPLETED] {
            let entry: AuditEntry = serde_json::from_str(line).unwrap();
            assert_eq!(serde_json::to_string(&entry).unwrap(), line);
        }
    }
}
