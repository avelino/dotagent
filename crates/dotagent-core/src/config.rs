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
}
