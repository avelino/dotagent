//! `[security]` section of the manifest.
//!
//! v0 is **schema-only**: dotagent parses these fields and `doctor` reports
//! inconsistencies, but the runner does not yet enforce them. Real sandbox
//! integration (`sandbox-exec` on macOS, `bubblewrap` / `firejail` on Linux)
//! lands as a follow-up — see `docs/security/threat-model.md`.
//!
//! Declaring intent in the manifest still has value today: it forces the
//! agent author to think through the blast radius, surfaces the surface
//! area to reviewers, and gives `doctor` something concrete to audit.

use serde::{Deserialize, Serialize};

/// Per-agent security declaration. Absent = no constraints (v0 default).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SecurityConfig {
    /// Whitelist of commands the agent is allowed to spawn (matches against
    /// `RunConfig::command`). Empty = no whitelist.
    #[serde(default)]
    pub allowed_commands: Vec<String>,

    /// Whitelist of plugin names this agent may invoke. Empty = manifest's
    /// `[[preflight]]` / `[[on_*]]` plugins are implicitly allowed.
    #[serde(default)]
    pub allowed_plugins: Vec<String>,

    /// Network policy.
    #[serde(default)]
    pub network: NetworkPolicy,

    /// Directories the agent is allowed to write to. Empty = no restriction.
    /// `$AGENT_TMPDIR` and `$AGENT_HEARTBEAT_FILE` are always writable.
    #[serde(default)]
    pub filesystem_writable: Vec<String>,

    /// Environment variables to pass through. Empty = pass all (default
    /// behavior matches `EnvConfig::inherit = true`).
    #[serde(default)]
    pub env_passthrough: Vec<String>,
}

/// Network reachability policy. v0 schema only.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "lowercase", untagged)]
pub enum NetworkPolicy {
    Mode(NetworkMode),
    /// Allow-list of hosts (e.g., `["api.github.com", "acme.sentry.io"]`).
    Allowlist(Vec<String>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum NetworkMode {
    Allow,
    Deny,
}

impl Default for NetworkPolicy {
    fn default() -> Self {
        NetworkPolicy::Mode(NetworkMode::Allow)
    }
}

impl SecurityConfig {
    /// Has the author declared *anything* under `[security]`? Used by
    /// `doctor` to nudge users toward adding an intentional declaration.
    pub fn is_explicit(&self) -> bool {
        !self.allowed_commands.is_empty()
            || !self.allowed_plugins.is_empty()
            || !self.filesystem_writable.is_empty()
            || !self.env_passthrough.is_empty()
            || !matches!(self.network, NetworkPolicy::Mode(NetworkMode::Allow))
    }
}
