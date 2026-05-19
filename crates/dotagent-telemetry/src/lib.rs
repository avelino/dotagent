//! Tracing + OpenTelemetry wiring for the dotagent daemon.
//!
//! Layers (composed in [`init`]):
//!
//! 1. **JSON file appender** with daily rotation under
//!    `$DOTAGENT_HOME/logs/daemon/dotagent.log.YYYY-MM-DD`. Always on.
//! 2. **Stderr** in the configured format (`json` / `pretty` / `compact`)
//!    so launchd / systemd capture something legible too.
//! 3. **OpenTelemetry OTLP layer** (opt-in). Enabled when
//!    `[telemetry] otlp_endpoint` in `~/.config/dotagent/config.toml` is
//!    set. Exports spans via gRPC. Auth headers come from the standard
//!    `OTEL_EXPORTER_OTLP_HEADERS` env var, e.g.
//!    `OTEL_EXPORTER_OTLP_HEADERS="x-honeycomb-team=YOUR_KEY"`.
//!
//! The returned [`Guard`] keeps the non-blocking file writer alive and
//! shuts the OTel tracer provider down cleanly. Drop it on daemon exit.

pub mod retention;

use std::path::PathBuf;

use dotagent_core::config::{Config, TelemetryConfig};
use opentelemetry::trace::TracerProvider as _;
use opentelemetry::KeyValue;
use opentelemetry_otlp::WithExportConfig;
use opentelemetry_sdk::trace::TracerProvider;
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

/// Drop this when the daemon exits to flush buffered logs + spans.
pub struct Guard {
    _file_guard: WorkerGuard,
    tracer_provider: Option<TracerProvider>,
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

    let stderr_layer = tracing_subscriber::fmt::layer()
        .compact()
        .with_writer(std::io::stderr);

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
) -> Result<(opentelemetry_sdk::trace::Tracer, TracerProvider), TelemetryError> {
    let mut attrs = vec![
        KeyValue::new("service.name", cfg.service_name.clone()),
        KeyValue::new("service.version", env!("CARGO_PKG_VERSION")),
    ];
    for (k, v) in &cfg.resource {
        attrs.push(KeyValue::new(k.clone(), v.clone()));
    }
    let resource = Resource::new(attrs);

    let exporter = opentelemetry_otlp::SpanExporter::builder()
        .with_tonic()
        .with_endpoint(cfg.otlp_endpoint.clone())
        .build()
        .map_err(|e| TelemetryError::Otel(e.to_string()))?;

    let provider = TracerProvider::builder()
        .with_batch_exporter(exporter, opentelemetry_sdk::runtime::Tokio)
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
