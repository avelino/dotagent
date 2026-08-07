//! Tracing + OpenTelemetry wiring for the dotagent daemon.
//!
//! Layers (composed in [`init`]):
//!
//! 1. **JSON file appender** with daily rotation under
//!    `$DOTAGENT_HOME/logs/daemon/dotagent.log.YYYY-MM-DD`. Always on.
//!    This is the source of truth: it rotates daily and the retention
//!    sweeper compresses + expires it.
//! 2. **Stderr mirror**, compact format — **only when stderr is a
//!    terminal**. Under launchd / systemd stderr is a plain file that the
//!    init system holds open, so nothing can rotate it; mirroring there
//!    grew an unbounded duplicate of (1). Left quiet, that channel stays
//!    free for what only it can carry: panics, aborts, and anything that
//!    fails before this subscriber exists. Override with
//!    `DOTAGENT_LOG_STDERR=1` (force on, e.g. when the unit is rewired to
//!    journald) or `DOTAGENT_LOG_STDERR=0` (force off).
//!    ANSI colour follows the same TTY check and honours `NO_COLOR`.
//! 3. **OpenTelemetry OTLP layer** (opt-in). Enabled when
//!    `[telemetry] otlp_endpoint` in `~/.config/dotagent/config.toml` is
//!    set. Exports spans via gRPC. Auth headers come from the standard
//!    `OTEL_EXPORTER_OTLP_HEADERS` env var, e.g.
//!    `OTEL_EXPORTER_OTLP_HEADERS="x-honeycomb-team=YOUR_KEY"`.
//!
//! The returned [`Guard`] keeps the non-blocking file writer alive and
//! shuts the OTel tracer provider down cleanly. Drop it on daemon exit.

pub mod retention;

use std::io::IsTerminal;
use std::path::PathBuf;

use dotagent_core::config::{Config, TelemetryConfig};
use opentelemetry::trace::TracerProvider as _;
use opentelemetry::KeyValue;
use opentelemetry_otlp::WithExportConfig;
use opentelemetry_sdk::trace::SdkTracerProvider;
use opentelemetry_sdk::Resource;
use thiserror::Error;
use tracing_appender::non_blocking::WorkerGuard;
use tracing_appender::rolling::{RollingFileAppender, Rotation};
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::EnvFilter;

#[derive(Debug, Error)]
pub enum TelemetryError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("otel pipeline: {0}")]
    Otel(String),
}

/// Forces the stderr mirror layer on (`1`/`true`/`yes`/`on`) or off
/// (`0`/`false`/`no`/`off`), overriding TTY detection.
pub const STDERR_MIRROR_ENV: &str = "DOTAGENT_LOG_STDERR";

/// Whether to install the stderr mirror layer.
///
/// Default is "only on a terminal". A non-TTY stderr under launchd /
/// systemd is a file held open by the init system: no rotation, no
/// retention, and every event it receives is already in the rotating JSON
/// log. Keeping it quiet is what stops that file from growing forever.
fn stderr_mirror_enabled(is_tty: bool, forced: Option<&str>) -> bool {
    match forced.map(|v| v.trim().to_ascii_lowercase()).as_deref() {
        Some("1" | "true" | "yes" | "on") => true,
        Some("0" | "false" | "no" | "off") => false,
        _ => is_tty,
    }
}

/// Whether a stderr log writer may emit ANSI escapes. `NO_COLOR` set to any
/// non-empty value wins over TTY detection (https://no-color.org).
///
/// Public because the daemon is not the only writer: the CLI's lightweight
/// per-subcommand subscriber has to answer the same question, and two copies
/// of this rule would drift the first time one of them is fixed.
pub fn ansi_enabled(is_tty: bool, no_color: Option<&str>) -> bool {
    if no_color.is_some_and(|v| !v.is_empty()) {
        return false;
    }
    is_tty
}

/// Drop this when the daemon exits to flush buffered logs + spans.
pub struct Guard {
    _file_guard: WorkerGuard,
    tracer_provider: Option<SdkTracerProvider>,
}

impl Drop for Guard {
    fn drop(&mut self) {
        if let Some(tp) = self.tracer_provider.take() {
            let _ = tp.shutdown();
        }
    }
}

/// Initialize the global subscriber + (optionally) the OTel pipeline.
/// Call once at startup.
pub fn init(config: &Config, log_dir_override: Option<PathBuf>) -> Result<Guard, TelemetryError> {
    let log_dir = log_dir_override.unwrap_or_else(dotagent_state::paths::daemon_logs_dir);
    std::fs::create_dir_all(&log_dir)?;

    // 1) JSON file appender (rotates daily).
    let file_appender = RollingFileAppender::new(Rotation::DAILY, &log_dir, "dotagent.log");
    let (file_writer, file_guard) = tracing_appender::non_blocking(file_appender);

    let filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(&config.logging.level));

    let json_file = tracing_subscriber::fmt::layer()
        .json()
        .with_current_span(true)
        .with_span_list(true)
        .with_writer(file_writer);

    // Built inline (same reason as the OTel layer below) so `Option<L>`
    // still infers its `S` from the `registry()` chain.
    let stderr_is_tty = std::io::stderr().is_terminal();
    let ansi = ansi_enabled(stderr_is_tty, std::env::var("NO_COLOR").ok().as_deref());
    let stderr_layer = stderr_mirror_enabled(
        stderr_is_tty,
        std::env::var(STDERR_MIRROR_ENV).ok().as_deref(),
    )
    .then(|| {
        tracing_subscriber::fmt::layer()
            .compact()
            .with_ansi(ansi)
            .with_writer(std::io::stderr)
    });

    // 2) Optional OTel layer — built inline so the layer's `S` type
    //    parameter can be inferred from the `registry()` chain.
    let (tracer, tracer_provider) = if config.telemetry.is_enabled() {
        let (t, p) = build_otel_tracer(&config.telemetry)?;
        (Some(t), Some(p))
    } else {
        (None, None)
    };
    let otel_layer = tracer.map(|t| tracing_opentelemetry::layer().with_tracer(t));

    tracing_subscriber::registry()
        .with(filter)
        .with(json_file)
        .with(stderr_layer)
        .with(otel_layer)
        .init();

    Ok(Guard {
        _file_guard: file_guard,
        tracer_provider,
    })
}

fn build_otel_tracer(
    cfg: &TelemetryConfig,
) -> Result<(opentelemetry_sdk::trace::Tracer, SdkTracerProvider), TelemetryError> {
    let mut attrs = vec![
        KeyValue::new("service.name", cfg.service_name.clone()),
        KeyValue::new("service.version", env!("CARGO_PKG_VERSION")),
    ];
    for (k, v) in &cfg.resource {
        attrs.push(KeyValue::new(k.clone(), v.clone()));
    }
    let resource = Resource::builder().with_attributes(attrs).build();

    let exporter = opentelemetry_otlp::SpanExporter::builder()
        .with_tonic()
        .with_endpoint(cfg.otlp_endpoint.clone())
        .build()
        .map_err(|e| TelemetryError::Otel(e.to_string()))?;

    let provider = SdkTracerProvider::builder()
        .with_batch_exporter(exporter)
        .with_resource(resource)
        .build();

    let tracer = provider.tracer(cfg.service_name.clone());
    Ok((tracer, provider))
}

/// Convenience: load the global config and call [`init`].
pub fn init_from_default_config() -> Result<Guard, TelemetryError> {
    let cfg = Config::load(dotagent_state::paths::config_file()).unwrap_or_default();
    init(&cfg, None)
}

/// Build a per-agent JSON file appender (rotates daily). Returns the
/// non-blocking writer + worker guard.
pub fn per_agent_appender(
    agent: &str,
) -> Result<(tracing_appender::non_blocking::NonBlocking, WorkerGuard), TelemetryError> {
    let dir = dotagent_state::paths::agent_logs_dir(agent);
    std::fs::create_dir_all(&dir)?;
    let appender = RollingFileAppender::new(Rotation::DAILY, dir, format!("{agent}.log"));
    let (nb, guard) = tracing_appender::non_blocking(appender);
    Ok((nb, guard))
}

/// Plain path to today's per-agent log file.
pub fn agent_log_path(agent: &str) -> PathBuf {
    dotagent_state::paths::agent_logs_dir(agent).join(format!("{agent}.log"))
}

/// Plain path to today's daemon log file.
pub fn daemon_log_path() -> PathBuf {
    dotagent_state::paths::daemon_logs_dir().join("dotagent.log")
}

#[cfg(test)]
mod tests {
    use super::{ansi_enabled, stderr_mirror_enabled};

    #[test]
    fn stderr_mirror_follows_tty_by_default() {
        assert!(stderr_mirror_enabled(true, None));
        // The launchd / systemd case: stderr is a file nobody rotates.
        assert!(!stderr_mirror_enabled(false, None));
    }

    #[test]
    fn stderr_mirror_env_overrides_tty() {
        for on in ["1", "true", "yes", "on", " TRUE ", "On"] {
            assert!(stderr_mirror_enabled(false, Some(on)), "{on}");
        }
        for off in ["0", "false", "no", "off", "OFF"] {
            assert!(!stderr_mirror_enabled(true, Some(off)), "{off}");
        }
    }

    #[test]
    fn stderr_mirror_ignores_garbage_env() {
        assert!(stderr_mirror_enabled(true, Some("maybe")));
        assert!(!stderr_mirror_enabled(false, Some("maybe")));
        assert!(!stderr_mirror_enabled(false, Some("")));
    }

    #[test]
    fn ansi_off_when_not_a_terminal() {
        assert!(ansi_enabled(true, None));
        assert!(!ansi_enabled(false, None));
    }

    #[test]
    fn no_color_beats_tty() {
        assert!(!ansi_enabled(true, Some("1")));
        assert!(!ansi_enabled(true, Some("anything")));
        // Empty value is "unset" per the NO_COLOR spec.
        assert!(ansi_enabled(true, Some("")));
    }
}
