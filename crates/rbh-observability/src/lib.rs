//! Shared observability bootstrap for robinhood-rs binaries.
//!
//! Call [`init`] as the very first line in `main()`. The returned [`Guard`]
//! carries a reload handle so SIGHUP can change the log level at runtime
//! without restarting the process.

pub mod metrics;

use tracing_subscriber::EnvFilter;
use tracing_subscriber::fmt;
use tracing_subscriber::prelude::*;
use tracing_subscriber::reload;

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
    Json,
    Pretty,
}

/// Handle returned from [`init`]. Holding it keeps the subscriber alive and
/// exposes runtime filter changes for SIGHUP reloads.
pub struct Guard {
    reload_handle: reload::Handle<EnvFilter, tracing_subscriber::Registry>,
}

impl Guard {
    /// Parse `directive` (e.g. `"info"`, `"rbh_policy=debug"`) and atomically
    /// replace the active filter. Used by SIGHUP reload in the daemon.
    pub fn reload_filter(&self, directive: &str) -> Result<(), ObsError> {
        let new_filter = EnvFilter::try_new(directive)
            .map_err(|e| ObsError::InvalidFilter(e.to_string()))?;
        self.reload_handle
            .reload(new_filter)
            .map_err(|e| ObsError::SetGlobal(e.to_string()))
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ObsError {
    #[error("failed to set global subscriber: {0}")]
    SetGlobal(String),
    #[error("invalid filter directive: {0}")]
    InvalidFilter(String),
}

/// Initialize the global tracing subscriber. Must be called exactly once.
pub fn init(cfg: ObservabilityConfig) -> Result<Guard, ObsError> {
    let filter = EnvFilter::try_new(&cfg.level)
        .map_err(|e| ObsError::InvalidFilter(e.to_string()))?;
    let (filter, reload_handle) = reload::Layer::new(filter);

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
            tracing::subscriber::set_global_default(subscriber)
                .map_err(|e| ObsError::SetGlobal(e.to_string()))?;
        }
        LogFormat::Pretty => {
            let subscriber = tracing_subscriber::registry()
                .with(filter)
                .with(fmt::layer().pretty().with_writer(std::io::stderr));
            tracing::subscriber::set_global_default(subscriber)
                .map_err(|e| ObsError::SetGlobal(e.to_string()))?;
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

    Ok(Guard { reload_handle })
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
    fn invalid_filter_rejected() {
        let cfg = ObservabilityConfig {
            level: "!!invalid!!".into(),
            format: LogFormat::Pretty,
            otlp_endpoint: None,
            service_name: "test",
        };
        assert!(matches!(init(cfg), Err(ObsError::InvalidFilter(_))));
    }
}
