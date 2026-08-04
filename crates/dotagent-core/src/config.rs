//! Global dotagent configuration — `~/.config/dotagent/config.toml`.
//!
//! Optional. When absent, dotagent uses the defaults baked into
//! [`Config::default`]. Override anything by writing a partial TOML; missing
//! fields fall back to defaults.
//!
//! ```toml
//! # ~/.config/dotagent/config.toml — full example with every field
//!
//! [logging]
//! level = "info"            # tracing filter (off, error, warn, info, debug, trace)
//! format = "json"           # "json" | "pretty" | "compact"
//! retention_days = 30       # daemon logs; older files are deleted
//! per_agent_retention_days = 14
//! compress_after_days = 1   # gzip rotated files older than N days
//!
//! [telemetry]
//! # Empty / absent = OTel disabled (default).
//! otlp_endpoint = ""        # e.g., "https://api.honeycomb.io:443"
//! protocol = "grpc"         # "grpc" | "http"
//! service_name = "dotagent"
//!
//! [telemetry.headers]
//! # Custom headers (per-vendor auth). All values are sent verbatim.
//! "x-honeycomb-team" = "your-api-key"
//!
//! [telemetry.resource]
//! # Resource attributes attached to every span/log.
//! "deployment.environment" = "production"
//! ```

use std::collections::BTreeMap;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub logging: LoggingConfig,
    #[serde(default)]
    pub telemetry: TelemetryConfig,
    #[serde(default)]
    pub secrets: SecretsConfig,
    #[serde(default)]
    pub telegram: TelegramIngressConfig,
    #[serde(default)]
    pub memory: MemoryConfig,
}

/// Long-term memory for agents, stored in an embedded outl workspace.
///
/// On by default with a workspace under `$DOTAGENT_HOME/outl`, scaffolded on
/// first use. The alternative — memory that stays broken until someone reads a
/// doc and runs `outl init` — is the kind of default this project avoids.
///
/// ```toml
/// [memory]
/// workspace = "/Users/me/outl-p2p"   # share the workspace with your notes
/// enabled = false                    # or turn it off entirely
/// ```
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryConfig {
    /// Expose memory tools from `dotagent mcp`.
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Workspace path. Empty (default) resolves to `$DOTAGENT_HOME/outl`.
    ///
    /// Pointing this at a workspace you already use puts what an agent
    /// remembers next to your own notes, synced to your peers. Convenient, and
    /// also means an agent writes where you write.
    #[serde(default)]
    pub workspace: String,
}

fn default_true() -> bool {
    true
}

impl Default for MemoryConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            workspace: String::new(),
        }
    }
}

impl MemoryConfig {
    /// Configured workspace, or `None` to mean "use the default path".
    pub fn workspace_override(&self) -> Option<&str> {
        let w = self.workspace.trim();
        (!w.is_empty()).then_some(w)
    }
}

/// Inbound Telegram. Off unless configured — dotagent never opens a network
/// listener you did not ask for.
///
/// **Daemon-level, not per-manifest, on purpose.** Telegram allows exactly one
/// `getUpdates` consumer per bot token; N manifests each polling would fight
/// over the same offset and silently drop each other's messages.
///
/// ```toml
/// [telegram]
/// bot_token = "${TELEGRAM_BOT_TOKEN}"
/// allowed_user_ids = [123456789]
/// dispatcher_agent = "telegram-assistant"
/// ```
///
/// Security posture, spelled out in `docs/security/threat-model.md`:
/// enabling this means a message from the public internet can cause a local
/// process to run. `allowed_user_ids` is the whole gate, so an empty list
/// refuses everyone rather than allowing everyone.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TelegramIngressConfig {
    /// Bot token. Accepts `${VAR}`, resolved against the secrets store at poll
    /// time so it never sits in `config.toml`.
    #[serde(default)]
    pub bot_token: String,
    /// Numeric Telegram user ids allowed to trigger runs.
    ///
    /// Numeric on purpose: `@username` is changeable and spoofable, a numeric
    /// id is not. Empty means nobody, which is why the ingress refuses to start
    /// with an empty list instead of treating it as "no restriction".
    #[serde(default)]
    pub allowed_user_ids: Vec<i64>,
    /// Agent that receives every accepted message.
    #[serde(default = "default_dispatcher_agent")]
    pub dispatcher_agent: String,
    /// Seconds to hold the `getUpdates` long-poll open. Telegram caps this at
    /// 50; the default trades one connection per 30s for near-instant replies
    /// without a webhook, a public IP or a tunnel.
    #[serde(default = "default_poll_timeout_seconds")]
    pub poll_timeout_seconds: u32,
    /// Max accepted messages per user per minute. Excess is dropped with an
    /// audit entry — the bot is reachable from anywhere, so an unbounded
    /// sender could otherwise keep the daemon busy indefinitely.
    #[serde(default = "default_rate_limit_per_minute")]
    pub rate_limit_per_minute: u32,
}

fn default_dispatcher_agent() -> String {
    "telegram-assistant".into()
}
fn default_poll_timeout_seconds() -> u32 {
    30
}
fn default_rate_limit_per_minute() -> u32 {
    10
}

impl Default for TelegramIngressConfig {
    fn default() -> Self {
        Self {
            bot_token: String::new(),
            allowed_user_ids: Vec::new(),
            dispatcher_agent: default_dispatcher_agent(),
            poll_timeout_seconds: default_poll_timeout_seconds(),
            rate_limit_per_minute: default_rate_limit_per_minute(),
        }
    }
}

impl TelegramIngressConfig {
    /// Whether the daemon should start the poller.
    ///
    /// Requires both a token and at least one allowed user. A configured token
    /// with an empty allowlist is treated as misconfiguration and stays off:
    /// the alternative reading ("allow everyone") turns a typo into an open
    /// remote-execution endpoint.
    pub fn is_enabled(&self) -> bool {
        !self.bot_token.is_empty() && !self.allowed_user_ids.is_empty()
    }

    /// Whether this user id may trigger runs.
    pub fn allows(&self, user_id: i64) -> bool {
        self.allowed_user_ids.contains(&user_id)
    }
}

/// Daemon-level secrets file override. See
/// [`docs/concepts/secrets.md`](../../../docs/concepts/secrets.md) for the
/// full posture; this struct only carries the path override.
///
/// The default (empty `file`) resolves to `$DOTAGENT_HOME/secrets.env` —
/// you only need this section when you want the file somewhere else (for
/// example, mounted from a secret manager into `/run/secrets/dotagent.env`).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SecretsConfig {
    /// Override the path to the secrets file. Empty (default) means the
    /// resolver in `dotagent-state::paths::secrets_file` is used (which
    /// itself honors `DOTAGENT_SECRETS_FILE`).
    #[serde(default)]
    pub file: String,
}

impl SecretsConfig {
    pub fn is_set(&self) -> bool {
        !self.file.is_empty()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LoggingConfig {
    #[serde(default = "default_level")]
    pub level: String,
    #[serde(default = "default_format")]
    pub format: String,
    #[serde(default = "default_retention_days")]
    pub retention_days: u32,
    #[serde(default = "default_per_agent_retention_days")]
    pub per_agent_retention_days: u32,
    #[serde(default = "default_compress_after_days")]
    pub compress_after_days: u32,
}

fn default_level() -> String {
    "info".into()
}
fn default_format() -> String {
    "json".into()
}
fn default_retention_days() -> u32 {
    30
}
fn default_per_agent_retention_days() -> u32 {
    14
}
fn default_compress_after_days() -> u32 {
    1
}

impl Default for LoggingConfig {
    fn default() -> Self {
        Self {
            level: default_level(),
            format: default_format(),
            retention_days: default_retention_days(),
            per_agent_retention_days: default_per_agent_retention_days(),
            compress_after_days: default_compress_after_days(),
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TelemetryConfig {
    /// OTLP endpoint (gRPC or HTTP). Empty = disabled.
    #[serde(default)]
    pub otlp_endpoint: String,
    /// `"grpc"` (default) or `"http"`.
    #[serde(default = "default_protocol")]
    pub protocol: String,
    /// `service.name` attribute on every span/log.
    #[serde(default = "default_service_name")]
    pub service_name: String,
    /// Extra resource attributes (e.g., `deployment.environment = "prod"`).
    #[serde(default)]
    pub resource: BTreeMap<String, String>,
    /// Headers attached to every OTLP request (auth tokens, vendor keys).
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
}

fn default_protocol() -> String {
    "grpc".into()
}
fn default_service_name() -> String {
    "dotagent".into()
}

impl TelemetryConfig {
    pub fn is_enabled(&self) -> bool {
        !self.otlp_endpoint.is_empty()
    }
}

impl Config {
    /// Load from a specific path. Returns `Default::default()` if the file
    /// doesn't exist (no error — config is optional).
    pub fn load(path: impl AsRef<Path>) -> Result<Self> {
        let p = path.as_ref();
        if !p.exists() {
            return Ok(Self::default());
        }
        let raw = std::fs::read_to_string(p)?;
        let cfg: Config = toml::from_str(&raw)
            .map_err(|e| Error::InvalidManifest(format!("config.toml parse error: {e}")))?;
        Ok(cfg)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn defaults_are_sane() {
        let c = Config::default();
        assert_eq!(c.logging.level, "info");
        assert_eq!(c.logging.format, "json");
        assert_eq!(c.logging.retention_days, 30);
        assert!(!c.telemetry.is_enabled());
    }

    #[test]
    fn missing_file_returns_default() {
        let dir = tempdir().unwrap();
        let c = Config::load(dir.path().join("nope.toml")).unwrap();
        assert_eq!(c.logging.level, "info");
    }

    #[test]
    fn partial_toml_uses_defaults_for_rest() {
        let dir = tempdir().unwrap();
        let p = dir.path().join("config.toml");
        std::fs::write(&p, "[logging]\nlevel = \"debug\"\n").unwrap();
        let c = Config::load(&p).unwrap();
        assert_eq!(c.logging.level, "debug");
        assert_eq!(c.logging.format, "json"); // default
        assert_eq!(c.logging.retention_days, 30); // default
    }

    #[test]
    fn secrets_file_override_parses() {
        let dir = tempdir().unwrap();
        let p = dir.path().join("config.toml");
        std::fs::write(&p, "[secrets]\nfile = \"/etc/dotagent/s.env\"\n").unwrap();
        let c = Config::load(&p).unwrap();
        assert!(c.secrets.is_set());
        assert_eq!(c.secrets.file, "/etc/dotagent/s.env");
    }

    #[test]
    fn secrets_default_is_empty() {
        let c = Config::default();
        assert!(!c.secrets.is_set());
        assert_eq!(c.secrets.file, "");
    }

    #[test]
    fn telemetry_disabled_by_default() {
        let dir = tempdir().unwrap();
        let p = dir.path().join("config.toml");
        std::fs::write(&p, "[telemetry]\nservice_name = \"x\"\n").unwrap();
        let c = Config::load(&p).unwrap();
        assert!(!c.telemetry.is_enabled());
        assert_eq!(c.telemetry.service_name, "x");
    }

    // --- telegram ingress: the gate that decides whether a message from the
    // public internet can start a local process. Deny cases dominate. ---

    #[test]
    fn telegram_is_off_by_default() {
        assert!(!Config::default().telegram.is_enabled());
    }

    #[test]
    fn telegram_stays_off_without_a_token() {
        let c = TelegramIngressConfig {
            allowed_user_ids: vec![1],
            ..Default::default()
        };
        assert!(!c.is_enabled());
    }

    #[test]
    fn telegram_stays_off_with_an_empty_allowlist() {
        // The dangerous reading of "empty" is "allow everyone". A token with
        // no allowlist is a typo away from an open execution endpoint, so it
        // must stay off.
        let c = TelegramIngressConfig {
            bot_token: "t".into(),
            allowed_user_ids: vec![],
            ..Default::default()
        };
        assert!(!c.is_enabled());
    }

    #[test]
    fn telegram_enables_only_with_both_token_and_allowlist() {
        let c = TelegramIngressConfig {
            bot_token: "t".into(),
            allowed_user_ids: vec![7],
            ..Default::default()
        };
        assert!(c.is_enabled());
    }

    #[test]
    fn telegram_denies_unlisted_users() {
        let c = TelegramIngressConfig {
            bot_token: "t".into(),
            allowed_user_ids: vec![7, 9],
            ..Default::default()
        };
        assert!(!c.allows(8));
        assert!(!c.allows(-7), "sign must matter");
        assert!(!c.allows(0));
        assert!(c.allows(7));
        assert!(c.allows(9));
    }

    #[test]
    fn telegram_denies_everyone_when_allowlist_empty() {
        let c = TelegramIngressConfig::default();
        assert!(!c.allows(1));
        assert!(!c.allows(123456789));
    }

    #[test]
    fn telegram_parses_from_toml_with_defaults() {
        let dir = tempdir().unwrap();
        let p = dir.path().join("config.toml");
        std::fs::write(
            &p,
            "[telegram]\nbot_token = \"${TG}\"\nallowed_user_ids = [42]\n",
        )
        .unwrap();
        let c = Config::load(&p).unwrap();
        assert!(c.telegram.is_enabled());
        assert_eq!(c.telegram.dispatcher_agent, "telegram-assistant");
        assert_eq!(c.telegram.poll_timeout_seconds, 30);
        assert_eq!(c.telegram.rate_limit_per_minute, 10);
        assert!(c.telegram.allows(42));
    }

    #[test]
    fn absent_telegram_section_leaves_ingress_off() {
        let dir = tempdir().unwrap();
        let p = dir.path().join("config.toml");
        std::fs::write(&p, "[logging]\nlevel = \"debug\"\n").unwrap();
        let c = Config::load(&p).unwrap();
        assert!(!c.telegram.is_enabled());
    }
}
