//! Unix signal handling for the daemon.
//!
//! * `SIGTERM` / `SIGINT` — trigger graceful shutdown via `CancellationToken`.
//! * `SIGHUP`             — reload log level (from env `RBH_LOG`) and invoke
//!   any installed reload callback (e.g. MDT rescan).
//! * `SIGUSR1`            — dump runtime stats to `tracing::info!`.

use std::sync::Arc;

use rbh_observability::Guard as ObsGuard;
use tokio::signal::unix::{SignalKind, signal};
use tokio_util::sync::CancellationToken;

/// Called on SIGHUP after the log filter has been reloaded. Return `Err`
/// to log (does not abort reload).
pub type ReloadHook = Arc<dyn Fn() -> anyhow::Result<()> + Send + Sync>;

/// Called on SIGUSR1. Should log a snapshot synchronously.
pub type DumpHook = Arc<dyn Fn() + Send + Sync>;

/// Spawn the signal supervisor. Returns when one of {SIGTERM, SIGINT} fires
/// and the caller should shut down. The `cancel` token is flipped before
/// returning.
pub async fn supervise(
    obs: ObsGuard, cancel: CancellationToken, reload_hook: Option<ReloadHook>, dump_hook: Option<DumpHook>,
) -> anyhow::Result<()> {
    let mut sigterm = signal(SignalKind::terminate())?;
    let mut sigint = signal(SignalKind::interrupt())?;
    let mut sighup = signal(SignalKind::hangup())?;
    let mut sigusr1 = signal(SignalKind::user_defined1())?;

    tracing::info!("signal supervisor ready (SIGHUP=reload SIGUSR1=dump SIGTERM/SIGINT=shutdown)");

    loop {
        tokio::select! {
            _ = sigterm.recv() => {
                tracing::info!("SIGTERM received — graceful shutdown");
                cancel.cancel();
                return Ok(());
            }
            _ = sigint.recv() => {
                tracing::info!("SIGINT received — graceful shutdown");
                cancel.cancel();
                return Ok(());
            }
            _ = sighup.recv() => {
                let directive = std::env::var("RBH_LOG")
                    .or_else(|_| std::env::var("RUST_LOG"))
                    .unwrap_or_else(|_| "info".to_string());
                match obs.reload_filter(&directive) {
                    Ok(()) => tracing::info!(directive = %directive, "SIGHUP: log filter reloaded"),
                    Err(e) => tracing::warn!(error = %e, "SIGHUP: filter reload failed"),
                }
                if let Some(hook) = &reload_hook
                    && let Err(e) = hook()
                {
                    tracing::warn!(error = %e, "SIGHUP: reload hook failed");
                }
            }
            _ = sigusr1.recv() => {
                tracing::info!("SIGUSR1 received — dumping stats");
                if let Some(hook) = &dump_hook {
                    hook();
                } else {
                    tracing::info!("no dump hook installed");
                }
            }
        }
    }
}
