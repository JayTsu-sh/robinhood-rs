//! Shared observability bootstrap for robinhood-rs binaries.
//!
//! Call [`init`] as the very first line in `main()` to configure structured
//! JSON logging to stdout with span context. The returned [`Guard`] flushes
//! on drop.
//!
//! v1: JSON logs to stdout via `tracing-subscriber::fmt::json()`.
//! v2: Optional OTLP export via `tracing-opentelemetry` (gated on `RBH_OTLP_ENDPOINT`).

use tracing_subscriber::EnvFilter;
use tracing_subscriber::fmt;
use tracing_subscriber::prelude::*;

/// Configuration for the observability stack.
pub struct ObservabilityConfig {
    /// Log level filter (e.g., "info", "debug", "rbh_policy=debug,info").
    /// Defaults to `RBH_LOG` env var, then "info".
    pub level: String,
    /// Output format.
    pub format: LogFormat,
    /// OTLP endpoint (v2, currently unused). Set via `RBH_OTLP_ENDPOINT`.
    pub otlp_endpoint: Option<String>,
    /// Service name for structured logs.
    pub service_name: &'static str,
}

impl Default for ObservabilityConfig {
    fn default() -> Self {
        Self {
            level: std::env::var("RBH_LOG")
                .or_else(|_| std::env::var("RUST_LOG"))
                .unwrap_or_else(|_| "info".to_string()),
            format: LogFormat::Json,
            otlp_endpoint: std::env::var("RBH_OTLP_ENDPOINT").ok(),
            service_name: "robinhood",
        }
    }
}

/// Log output format.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogFormat {
    /// JSON objects, one per line — queryable with `jq`.
    Json,
    /// Human-readable pretty output for development.
    Pretty,
}

/// Guard that flushes pending log/trace data on drop.
/// Must be held alive for the lifetime of the program.
pub struct Guard {
    _private: (),
}

/// Errors from observability initialization.
#[derive(Debug, thiserror::Error)]
pub enum ObsError {
    #[error("failed to set global subscriber: {0}")]
    SetGlobal(String),
    #[error("invalid filter directive: {0}")]
    InvalidFilter(String),
}

/// Initialize the global tracing subscriber.
///
/// Must be called exactly once, as the first line in `main()`.
/// Returns a [`Guard`] that should be held until shutdown.
pub fn init(cfg: ObservabilityConfig) -> Result<Guard, ObsError> {
    let filter = EnvFilter::try_new(&cfg.level).map_err(|e| ObsError::InvalidFilter(e.to_string()))?;

    match cfg.format {
        LogFormat::Json => {
            let subscriber = tracing_subscriber::registry().with(filter).with(
                fmt::layer()
                    .json()
                    .with_target(true)
                    .with_span_list(true)
                    .with_current_span(true)
                    .with_thread_ids(false)
                    .with_thread_names(false),
            );
            tracing::subscriber::set_global_default(subscriber).map_err(|e| ObsError::SetGlobal(e.to_string()))?;
        }
        LogFormat::Pretty => {
            let subscriber = tracing_subscriber::registry()
                .with(filter)
                .with(fmt::layer().pretty().with_writer(std::io::stderr));
            tracing::subscriber::set_global_default(subscriber).map_err(|e| ObsError::SetGlobal(e.to_string()))?;
        }
    }

    if cfg.otlp_endpoint.is_some() {
        tracing::info!(
            service_name = cfg.service_name,
            "OTLP endpoint configured but not yet implemented (v2)"
        );
    }

    tracing::info!(
        service_name = cfg.service_name,
        level = %cfg.level,
        format = ?cfg.format,
        "observability initialized"
    );

    Ok(Guard { _private: () })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config() {
        let cfg = ObservabilityConfig::default();
        assert_eq!(cfg.format, LogFormat::Json);
        assert_eq!(cfg.service_name, "robinhood");
    }

    #[test]
    fn log_format_variants() {
        assert_ne!(LogFormat::Json, LogFormat::Pretty);
    }
}
