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
//! [state]
//! window_retention_days = 30   # state/windows/*.json; 0 keeps them forever
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
//!
//! [daily_summary]
//! time = "22:45"            # local HH:MM the daemon delivers the summary
//! grace_minutes = 30        # how late a wake-up is still allowed to deliver
//! enabled = true
//!
//! [[daily_summary.notifiers]]
//! # Same shape as a manifest's `[[notifiers]]`. Absent = desktop.
//! driver = "telegram"
//! bot_token = "${TELEGRAM_BOT_TOKEN}"
//! chat_id = "123456789"
//! ```

use std::collections::BTreeMap;
use std::path::Path;

use chrono::NaiveTime;
use dotagent_notify::NotifierEntry;
use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub logging: LoggingConfig,
    #[serde(default)]
    pub state: StateConfig,
    #[serde(default)]
    pub telemetry: TelemetryConfig,
    #[serde(default)]
    pub secrets: SecretsConfig,
    #[serde(default)]
    pub telegram: TelegramIngressConfig,
    #[serde(default)]
    pub memory: MemoryConfig,
    #[serde(default)]
    pub skills: SkillsConfig,
    #[serde(default)]
    pub commands: CommandsConfig,
    #[serde(default)]
    pub daily_summary: DailySummaryConfig,
    #[serde(default)]
    pub power: crate::power::PowerConfig,
}

/// End-of-day health summary — when it goes out, and to whom.
///
/// Works with no config file: on by default, at 22:45 local, delivered to the
/// `desktop` driver. Desktop is the fallback because it is the only driver
/// with nothing to fill in and nothing to leak — no credential, no network,
/// no address that could be wrong. Every other destination needs a chat id, a
/// phone number or a webhook, and there is no universal default for those.
///
/// ```toml
/// [daily_summary]
/// time = "07:30"        # a morning report reads better than a bedtime one
/// grace_minutes = 60    # laptop opens late; still deliver
/// enabled = false       # or drop it entirely
///
/// [[daily_summary.notifiers]]
/// driver = "telegram"
/// bot_token = "${TELEGRAM_BOT_TOKEN}"
/// chat_id = "123456789"
/// ```
///
/// `notifiers` takes the same entries a manifest's `[[notifiers]]` takes,
/// including `driver = "plugin"`. Listing more than one delivers to all of
/// them.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DailySummaryConfig {
    /// Deliver the summary at all.
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Local time of day, `HH:MM` (or `HH:MM:SS`).
    ///
    /// A value that does not parse falls back to [`DEFAULT_DAILY_SUMMARY_TIME`]
    /// rather than disabling delivery: a typo here should cost you a wrong
    /// hour, not a silent month.
    #[serde(default = "default_daily_summary_time")]
    pub time: String,
    /// How long after [`Self::time`] a delivery still counts.
    ///
    /// The daemon schedules a wake-up for `time`, so under normal operation
    /// this is unused. It covers the cases where the wake-up cannot happen at
    /// all — laptop asleep, machine off, a tick that overran its own sleep
    /// budget. Clamped to at least 1 minute, because a zero-width window is
    /// an empty one and would silently deliver nothing.
    #[serde(default = "default_daily_summary_grace")]
    pub grace_minutes: u32,
    /// Where the summary goes. Empty means the `desktop` driver.
    #[serde(default)]
    pub notifiers: Vec<NotifierEntry>,
}

/// Time of day used when `[daily_summary].time` is absent or unparseable.
pub const DEFAULT_DAILY_SUMMARY_TIME: NaiveTime = match NaiveTime::from_hms_opt(22, 45, 0) {
    Some(t) => t,
    None => unreachable!(),
};

fn default_daily_summary_time() -> String {
    "22:45".into()
}
fn default_daily_summary_grace() -> u32 {
    30
}

impl Default for DailySummaryConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            time: default_daily_summary_time(),
            grace_minutes: default_daily_summary_grace(),
            notifiers: Vec::new(),
        }
    }
}

impl DailySummaryConfig {
    /// Parsed [`Self::time`], or `None` when it is not a valid time of day.
    pub fn time_of_day(&self) -> Option<NaiveTime> {
        let raw = self.time.trim();
        NaiveTime::parse_from_str(raw, "%H:%M:%S")
            .or_else(|_| NaiveTime::parse_from_str(raw, "%H:%M"))
            .ok()
    }

    /// [`Self::time_of_day`] with the documented fallback applied.
    pub fn time_or_default(&self) -> NaiveTime {
        self.time_of_day().unwrap_or(DEFAULT_DAILY_SUMMARY_TIME)
    }

    /// Grace window in minutes, never zero.
    pub fn effective_grace_minutes(&self) -> u32 {
        self.grace_minutes.clamp(1, 24 * 60)
    }
}

/// Commands — procedures a human invokes by name, published as a Telegram menu.
///
/// On by default with an empty catalog, which costs nothing: no commands
/// installed means no menu registered and no tools listed.
///
/// ```toml
/// [commands]
/// enabled = false                 # drop commands entirely
/// claude_commands = true          # also search ~/.claude/commands
/// paths = ["/opt/team-commands"]  # extra roots, searched first
/// ```
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CommandsConfig {
    /// Register the Telegram menu and expose the `command-*` tools.
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Also search `~/.claude/commands` and `$CWD/.claude/commands`.
    ///
    /// **Off by default, unlike [`SkillsConfig::claude_skills`].** The two
    /// catalogs are not equally safe to inherit. A skill is loaded only when a
    /// model judges it relevant, so an irrelevant one costs a line in a list. A
    /// command is *published as a menu* — and a Claude Code command catalog is
    /// typically full of things that assume a shell and a working directory
    /// (`/apply` switching a Nix profile, `/dx` opening a terminal layout).
    /// Publishing those to a chat produces menu entries that cannot work, which
    /// is worse than not listing them.
    ///
    /// Turn it on when the catalog is written for an assistant rather than for
    /// a terminal.
    #[serde(default)]
    pub claude_commands: bool,
    /// Extra directories to scan, each holding one `.md` file per command.
    /// Searched before the defaults, so a name declared here wins.
    #[serde(default)]
    pub paths: Vec<String>,
}

impl Default for CommandsConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            claude_commands: false,
            paths: Vec::new(),
        }
    }
}

/// Skills — procedures exposed to MCP clients as `skill-*` tools.
///
/// On by default with an empty catalog, which costs nothing: no skills
/// installed means no tools listed.
///
/// ```toml
/// [skills]
/// enabled = false                  # drop skill tools entirely
/// claude_skills = false            # stop searching ~/.claude/skills
/// paths = ["/opt/team-skills"]     # extra roots, searched first
/// ```
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkillsConfig {
    /// Expose skill tools from `dotagent mcp`.
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Also search `~/.claude/skills` and `$CWD/.claude/skills`.
    ///
    /// On by default: a skill written for Claude Code is the common case, and
    /// asking someone to copy or symlink it first would make the feature look
    /// broken on arrival. Turn it off when the Claude Code catalog is large
    /// and mostly irrelevant to an assistant that has no shell.
    #[serde(default = "default_true")]
    pub claude_skills: bool,
    /// Extra directories to scan, each holding one subdirectory per skill.
    /// Searched before the defaults, so a name declared here wins.
    #[serde(default)]
    pub paths: Vec<String>,
}

impl Default for SkillsConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            claude_skills: true,
            paths: Vec::new(),
        }
    }
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

/// Retention for what dotagent writes under `state/`.
///
/// Works with no config file: the defaults already bound the only directory
/// that grows without limit. A schedule produces one window file per fired
/// window and never revisits it, so a 15-minute agent alone leaves ~96 files
/// a day behind — pairs of `.json` + `.lock` that nothing ever reclaimed
/// before this existed.
///
/// ```toml
/// [state]
/// window_retention_days = 90   # keep a quarter of retry history
/// # window_retention_days = 0  # or keep everything, forever
/// ```
///
/// Heartbeats are deliberately not covered: there is exactly one per
/// `(agent, schedule)` and it is rewritten in place, so the directory is
/// bounded by how many schedules exist.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StateConfig {
    /// Days to keep `state/windows/<agent>-<slug>-<window>.json` (and the
    /// `.lock` beside it). `0` disables the sweep entirely.
    ///
    /// The horizon has to clear the oldest window the daemon might still
    /// consult. A window stops being actionable once it is older than the
    /// schedule's `stale_after_minutes` (default 120), which the default here
    /// exceeds by ~360×. Even a wildly permissive `stale_after_minutes` of a
    /// full week still has four days of headroom. Erring high costs a few MB;
    /// erring low deletes retry state under a running daemon, which resets
    /// `attempts` and re-fires an alert someone already gave up on.
    #[serde(default = "default_window_retention_days")]
    pub window_retention_days: u32,
}

fn default_window_retention_days() -> u32 {
    30
}

impl Default for StateConfig {
    fn default() -> Self {
        Self {
            window_retention_days: default_window_retention_days(),
        }
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
    fn skills_are_on_by_default_including_the_claude_catalog() {
        let c = Config::default();
        assert!(c.skills.enabled);
        assert!(c.skills.claude_skills, "reuse is the default promise");
        assert!(c.skills.paths.is_empty());
    }

    #[test]
    fn skills_can_be_narrowed_from_toml() {
        let dir = tempdir().unwrap();
        let p = dir.path().join("config.toml");
        std::fs::write(
            &p,
            "[skills]\nclaude_skills = false\npaths = [\"/opt/team\"]\n",
        )
        .unwrap();
        let c = Config::load(&p).unwrap();
        assert!(c.skills.enabled, "unset fields keep their default");
        assert!(!c.skills.claude_skills);
        assert_eq!(c.skills.paths, vec!["/opt/team".to_string()]);
    }

    #[test]
    fn commands_are_on_by_default_but_the_claude_catalog_is_not() {
        // The asymmetry with skills is deliberate: a command is published as a
        // menu, and a terminal-shaped catalog would fill it with entries that
        // cannot work in a chat.
        let c = Config::default();
        assert!(c.commands.enabled);
        assert!(!c.commands.claude_commands);
        assert!(c.commands.paths.is_empty());
    }

    #[test]
    fn commands_can_be_widened_from_toml() {
        let dir = tempdir().unwrap();
        let p = dir.path().join("config.toml");
        std::fs::write(&p, "[commands]\nclaude_commands = true\n").unwrap();
        let c = Config::load(&p).unwrap();
        assert!(c.commands.enabled, "unset fields keep their default");
        assert!(c.commands.claude_commands);
    }

    // --- state retention: the knob that decides whether a window a retry
    // still depends on survives the nightly sweep. ---

    #[test]
    fn window_retention_works_without_a_config_file() {
        // The whole point: nobody should have to write config.toml to stop
        // state/windows from growing forever.
        let c = Config::default();
        assert_eq!(c.state.window_retention_days, 30);
    }

    #[test]
    fn window_retention_survives_an_unrelated_config_file() {
        let dir = tempdir().unwrap();
        let p = dir.path().join("config.toml");
        std::fs::write(&p, "[logging]\nlevel = \"debug\"\n").unwrap();
        let c = Config::load(&p).unwrap();
        assert_eq!(c.state.window_retention_days, 30);
    }

    #[test]
    fn window_retention_can_be_widened() {
        let dir = tempdir().unwrap();
        let p = dir.path().join("config.toml");
        std::fs::write(&p, "[state]\nwindow_retention_days = 90\n").unwrap();
        let c = Config::load(&p).unwrap();
        assert_eq!(c.state.window_retention_days, 90);
    }

    #[test]
    fn window_retention_zero_means_keep_everything() {
        // Zero is an opt-out, not "delete everything" — the reading that would
        // wipe live retry state must not be the one a typo selects.
        let dir = tempdir().unwrap();
        let p = dir.path().join("config.toml");
        std::fs::write(&p, "[state]\nwindow_retention_days = 0\n").unwrap();
        let c = Config::load(&p).unwrap();
        assert_eq!(c.state.window_retention_days, 0);
    }

    // --- daily summary: the section that decides whether the end-of-day
    // report reaches a human at all. It used to reach a hardcoded phone
    // number that belonged to nobody. ---

    #[test]
    fn daily_summary_works_without_a_config_file() {
        let c = Config::default();
        assert!(c.daily_summary.enabled);
        assert_eq!(
            c.daily_summary.time_or_default(),
            NaiveTime::from_hms_opt(22, 45, 0).unwrap()
        );
        assert_eq!(c.daily_summary.effective_grace_minutes(), 30);
        assert!(
            c.daily_summary.notifiers.is_empty(),
            "empty means the desktop fallback, resolved at delivery"
        );
    }

    #[test]
    fn daily_summary_time_parses_both_shapes() {
        let c = DailySummaryConfig {
            time: "07:05".into(),
            ..Default::default()
        };
        assert_eq!(c.time_of_day(), NaiveTime::from_hms_opt(7, 5, 0));
        let c = DailySummaryConfig {
            time: " 23:59:30 ".into(),
            ..Default::default()
        };
        assert_eq!(c.time_of_day(), NaiveTime::from_hms_opt(23, 59, 30));
    }

    #[test]
    fn daily_summary_bad_time_falls_back_instead_of_silencing() {
        // A typo must cost the wrong hour, not the whole feature.
        for bad in ["", "25:00", "22.45", "quarter to eleven", "22:60"] {
            let c = DailySummaryConfig {
                time: bad.into(),
                ..Default::default()
            };
            assert_eq!(c.time_of_day(), None, "{bad} should not parse");
            assert_eq!(c.time_or_default(), DEFAULT_DAILY_SUMMARY_TIME, "{bad}");
        }
    }

    #[test]
    fn daily_summary_grace_is_never_zero() {
        // grace_minutes = 0 would make the window half-open on itself: empty.
        // Delivery would stop with no error anywhere — the exact failure mode
        // this section exists to end.
        let c = DailySummaryConfig {
            grace_minutes: 0,
            ..Default::default()
        };
        assert_eq!(c.effective_grace_minutes(), 1);
    }

    #[test]
    fn daily_summary_grace_is_capped_at_a_day() {
        let c = DailySummaryConfig {
            grace_minutes: u32::MAX,
            ..Default::default()
        };
        assert_eq!(c.effective_grace_minutes(), 24 * 60);
    }

    #[test]
    fn daily_summary_parses_notifiers_from_toml() {
        let dir = tempdir().unwrap();
        let p = dir.path().join("config.toml");
        std::fs::write(
            &p,
            "[daily_summary]\ntime = \"07:30\"\n\n[[daily_summary.notifiers]]\ndriver = \"telegram\"\nbot_token = \"${TG}\"\nchat_id = \"42\"\n",
        )
        .unwrap();
        let c = Config::load(&p).unwrap();
        assert!(c.daily_summary.enabled, "unset fields keep their default");
        assert_eq!(
            c.daily_summary.time_of_day(),
            NaiveTime::from_hms_opt(7, 30, 0)
        );
        assert_eq!(c.daily_summary.notifiers.len(), 1);
        assert_eq!(c.daily_summary.notifiers[0].driver_name(), "telegram");
    }

    #[test]
    fn daily_summary_survives_an_unrelated_config_file() {
        let dir = tempdir().unwrap();
        let p = dir.path().join("config.toml");
        std::fs::write(&p, "[logging]\nlevel = \"debug\"\n").unwrap();
        let c = Config::load(&p).unwrap();
        assert!(c.daily_summary.enabled);
        assert_eq!(
            c.daily_summary.time_or_default(),
            DEFAULT_DAILY_SUMMARY_TIME
        );
    }

    #[test]
    fn daily_summary_can_be_turned_off() {
        let dir = tempdir().unwrap();
        let p = dir.path().join("config.toml");
        std::fs::write(&p, "[daily_summary]\nenabled = false\n").unwrap();
        let c = Config::load(&p).unwrap();
        assert!(!c.daily_summary.enabled);
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
