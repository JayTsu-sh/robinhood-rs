//! robinhood-rs daemon library.
//!
//! The public [`run`] function boots the full stack: observability, database,
//! changelog listener, fs-scan, scheduler-rs, and the axum HTTP server.

#![allow(clippy::items_after_test_module)]

mod changelog;
mod heat_agent;
mod heat_sampling;
pub use heat_agent::{HeatAgentError, run_heat_agent};
mod hsm_poller;
mod resource_collection;
mod runtime;
mod signals;

use std::sync::Arc;

use anyhow::Context;
use scheduler_rs::prelude::*;
use scheduler_rs::store::{DataStore, MemoryStore, MysqlStore};
use tokio_util::sync::CancellationToken;

/// Run the robinhood-rs daemon.
pub async fn run() -> anyhow::Result<()> {
    // 1. Observability — must be first.
    let obs_guard = rbh_observability::init(rbh_observability::ObservabilityConfig {
        service_name: "rbh-daemon",
        ..Default::default()
    })
    .context("failed to initialize observability")?;

    tracing::info!("robinhood-rs daemon starting");

    let writers = if let Some(path) = std::env::var_os("RBH_REV8_WRITERS_FILE") {
        let json = tokio::fs::read_to_string(path)
            .await
            .context("read RBH_REV8_WRITERS_FILE")?;
        rbh_api::Rev8Writers::from_digest_json(&json).map_err(anyhow::Error::msg)?
    } else {
        rbh_api::Rev8Writers::default()
    };

    // 2. Database connection.
    let db_url = std::env::var("RBH_DATABASE_URL").unwrap_or_else(|_| "mysql://root@127.0.0.1/rbh_entries".to_string());
    let pool = sqlx::MySqlPool::connect(&db_url)
        .await
        .context("failed to connect to MariaDB")?;

    // 3. Run migrations (entry-store owns all schema).
    let entry_store = rbh_entry_store::store::EntryStore::connect(&db_url)
        .await
        .context("failed to initialize entry store")?;
    let runtime_registry = runtime::RuntimeRegistry::from_env()?;
    for filesystem in runtime_registry.iter() {
        entry_store.register_filesystem(&filesystem.config).await?;
        tracing::info!(
            filesystem = %filesystem.config.id,
            backend = ?filesystem.config.backend,
            mount = %filesystem.config.mount_path.display(),
            capabilities = ?filesystem.config.capabilities,
            "filesystem runtime registered"
        );
    }
    let executor_mounts: Arc<[_]> = runtime_registry
        .iter()
        .filter(|runtime| runtime.config.backend == rbh_entry_store::BackendKind::Lustre)
        .map(|runtime| runtime.config.mount_path.clone())
        .collect();
    let mut policy_store = rbh_policy::PolicyStore::new(pool.clone())
        .with_configured_filesystems(runtime_registry.iter().map(|runtime| runtime.config.id.clone()));
    for filesystem in runtime_registry.iter() {
        if let Some(config) = &filesystem.read_heat {
            policy_store = policy_store
                .with_read_heat(filesystem.config.clone(), config.clone(), executor_mounts.clone())
                .with_context(|| format!("read_heat configuration for {}", filesystem.config.id))?;
        }
    }
    if let Some(path) = std::env::var_os("RBH_REV8_HSM_PRINCIPAL_FILE") {
        use rbh_policy::rev8::execution::{LustreHsm, Principal, Runtime};

        let json = tokio::fs::read_to_string(path)
            .await
            .context("read RBH_REV8_HSM_PRINCIPAL_FILE")?;
        let principal: Principal = serde_json::from_str(&json).context("invalid Rev.8 HSM workload identity")?;
        anyhow::ensure!(
            !principal.service_account.trim().is_empty(),
            "Rev.8 HSM serviceAccount is empty"
        );
        policy_store = policy_store.with_rev8_runtime(Runtime {
            principal,
            hsm: Arc::new(LustreHsm),
        });
    }
    policy_store
        .check_schema()
        .await
        .context("policy schema check failed")?;
    let lustre: Vec<_> = runtime_registry
        .iter()
        .filter(|runtime| runtime.config.backend == rbh_entry_store::BackendKind::Lustre)
        .collect();
    if let [runtime] = lustre.as_slice() {
        let rebound_cursors = entry_store
            .bind_legacy_lustre_cursors(&runtime.config.id)
            .await
            .context("failed to bind legacy changelog cursors to the sole configured Lustre filesystem")?;
        if rebound_cursors > 0 {
            tracing::info!(filesystem = %runtime.config.id, cursors = rebound_cursors, "legacy changelog cursors bound to sole Lustre filesystem");
        }
    } else if !lustre.is_empty() {
        tracing::warn!(
            lustre_filesystems = lustre.len(),
            "legacy unscoped changelog cursors are not rebound when multiple Lustre filesystems are configured"
        );
    }
    let classifier_store = rbh_policy::ClassifierStore::new(pool.clone());

    // Load classifier cache for changelog-driven classification.
    let classifier_cache: std::sync::Arc<tokio::sync::RwLock<Vec<rbh_policy::ClassifierRow>>> = std::sync::Arc::new(
        tokio::sync::RwLock::new(classifier_store.list().await.unwrap_or_default()),
    );
    tracing::info!("database migrations complete");

    // 4. Spawn one isolated scan/change-source supervisor per Lustre runtime.
    //
    // Env:
    //   RBH_MDTS             — comma-separated MDT names (e.g. "fs-MDT0000,fs-MDT0001")
    //                          Falls back to RBH_MDT_NAME for backward compat.
    //   RBH_CHANGELOG_USER   — either a single reader id reused on every MDT,
    //                          or a comma-separated list matching RBH_MDTS 1:1.
    let daemon_cancel = CancellationToken::new();
    let heat_intervals: std::collections::BTreeMap<_, _> = runtime_registry
        .iter()
        .filter_map(|runtime| {
            let profile = runtime.read_heat.as_ref()?;
            Some((
                runtime.config.id.clone(),
                std::time::Duration::from_secs(u64::from(profile.interval_seconds)),
            ))
        })
        .collect();
    if !heat_intervals.is_empty() {
        tokio::spawn(heat_sampling::supervise(
            policy_store.clone(),
            heat_intervals,
            daemon_cancel.clone(),
        ));
    }
    for runtime in runtime_registry
        .iter()
        .filter(|runtime| runtime.config.backend == rbh_entry_store::BackendKind::Lustre)
        .cloned()
    {
        let runtime_store = entry_store.clone();
        let runtime_cursor_store = Arc::new(rbh_entry_store::store::MariaDbCursorStore::new(
            pool.clone(),
            runtime.config.id.clone(),
        ));
        let runtime_cancel = daemon_cancel.clone();
        let runtime_classifiers = classifier_cache.clone();
        tokio::spawn(async move {
            let count = runtime_store.scoped_entry_count(&runtime.config.id).await.unwrap_or(0);
            if count == 0 {
                loop {
                    tracing::info!(filesystem = %runtime.config.id, backend = ?runtime.config.backend, mount = %runtime.config.mount_path.display(), "catalog empty — running isolated initial fs-scan");
                    match run_initial_scan(&runtime_store, &runtime.config.id, &runtime.config.mount_path).await {
                        Ok(()) => break,
                        Err(error) => {
                            tracing::warn!(filesystem = %runtime.config.id, backend = ?runtime.config.backend, %error, "initial fs-scan failed; retrying this filesystem");
                            tokio::select! {
                                () = runtime_cancel.cancelled() => return,
                                () = tokio::time::sleep(std::time::Duration::from_secs(5)) => {}
                            }
                        }
                    }
                }
                let new_count = runtime_store.scoped_entry_count(&runtime.config.id).await.unwrap_or(0);
                tracing::info!(filesystem = %runtime.config.id, backend = ?runtime.config.backend, entries = new_count, "initial scan complete");
            } else {
                tracing::info!(filesystem = %runtime.config.id, backend = ?runtime.config.backend, entries = count, "catalog already populated");
            }
            if runtime.lustre_changelog.is_empty() {
                tracing::info!(filesystem = %runtime.config.id, backend = ?runtime.config.backend, "Lustre changelog listener disabled");
            }
            for changelog in &runtime.lustre_changelog {
                let mdt_name = changelog.mdt.clone();
                let reader_id = changelog.reader_id.clone();
                let ingest_store = runtime_store.clone();
                let ingest_mount = runtime.config.mount_path.clone();
                let ingest_filesystem = runtime.config.id.clone();
                let ingest_cancel = runtime_cancel.clone();
                let classifier_cache = runtime_classifiers.clone();
                let cursor_store = runtime_cursor_store.clone();
                let backend = runtime.config.backend;
                tokio::spawn(async move {
                    loop {
                        if ingest_cancel.is_cancelled() {
                            return;
                        }
                        let listener_cfg = lustre_changelog::ListenerConfig {
                            mdt: mdt_name.clone(),
                            reader_id: reader_id.clone(),
                            follow: true,
                            channel_buffer: 32,
                            ..Default::default()
                        };
                        match lustre_changelog::ChangelogListener::spawn(
                            listener_cfg,
                            cursor_store.clone(),
                            ingest_cancel.clone(),
                        )
                        .await
                        {
                            Ok(handle) => {
                                tracing::info!(filesystem = %ingest_filesystem, backend = ?backend, mdt = %mdt_name, reader_id = %reader_id, "changelog listener started");
                                let source =
                                    rbh_change_source::LustreChangeSource::new(ingest_filesystem.clone(), handle);
                                let exit = changelog::ingest_loop(
                                    Box::new(source),
                                    ingest_store.clone(),
                                    ingest_filesystem.clone(),
                                    ingest_mount.clone(),
                                    ingest_cancel.clone(),
                                    classifier_cache.clone(),
                                )
                                .await;
                                tracing::warn!(filesystem = %ingest_filesystem, backend = ?backend, mdt = %mdt_name, ?exit, "Lustre changelog runtime exited; restarting");
                            }
                            Err(error) => {
                                tracing::error!(filesystem = %ingest_filesystem, backend = ?backend, mdt = %mdt_name, reader_id = %reader_id, %error, "failed to start Lustre changelog runtime; retrying")
                            }
                        }
                        tokio::select! {
                            _ = ingest_cancel.cancelled() => return,
                            _ = tokio::time::sleep(std::time::Duration::from_secs(1)) => {}
                        }
                    }
                });
            }
        });
    }

    for runtime in runtime_registry
        .iter()
        .filter(|runtime| runtime.config.backend == rbh_entry_store::BackendKind::JuiceFs)
    {
        let Some(agent) = runtime.changelog_agent.as_ref() else {
            tracing::warn!(filesystem = %runtime.config.id, "JuiceFS runtime has no changelog_agent configuration");
            continue;
        };
        let ingest_store = entry_store.clone();
        let ingest_filesystem = runtime.config.id.clone();
        let ingest_mount = runtime.config.mount_path.clone();
        let ingest_cancel = daemon_cancel.clone();
        let classifier_cache = classifier_cache.clone();
        let endpoint = agent.endpoint.clone();
        let volume = agent.volume.clone();
        let backend = runtime.config.backend;
        tokio::spawn(async move {
            loop {
                if ingest_cancel.is_cancelled() {
                    break;
                }
                let baseline = match ingest_store.get_baseline(&ingest_filesystem).await {
                    Ok(baseline) => baseline,
                    Err(error) => {
                        tracing::error!(filesystem = %ingest_filesystem, backend = ?backend, %error, "failed to read JuiceFS baseline; retrying runtime");
                        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                        continue;
                    }
                };
                if baseline.as_ref().is_none_or(|value| {
                    matches!(
                        value.state,
                        rbh_entry_store::model::BaselineState::Invalid
                            | rbh_entry_store::model::BaselineState::Scanning
                    )
                }) && let Err(error) = run_juicefs_baseline(&ingest_store, &ingest_filesystem, &ingest_mount).await
                {
                    tracing::error!(filesystem = %ingest_filesystem, backend = ?backend, %error, "JuiceFS baseline scan failed; retrying runtime");
                    tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                    continue;
                }
                match rbh_change_source::JuiceFsChangeSource::connect(
                    ingest_filesystem.clone(),
                    endpoint.clone(),
                    volume.clone(),
                )
                .await
                {
                    Ok(source) => {
                        // Re-scan only after Watch is established. The first
                        // scan makes an idle mount available; this second pass
                        // closes its race window while the Agent buffers every
                        // changelog record produced during traversal.
                        if ingest_store
                            .get_baseline(&ingest_filesystem)
                            .await
                            .ok()
                            .flatten()
                            .is_some_and(|baseline| baseline.state == rbh_entry_store::model::BaselineState::CatchingUp)
                            && let Err(error) =
                                run_juicefs_baseline(&ingest_store, &ingest_filesystem, &ingest_mount).await
                        {
                            tracing::error!(filesystem = %ingest_filesystem, backend = ?backend, %error, "JuiceFS catch-up scan failed; restarting runtime");
                            continue;
                        }
                        let exit = changelog::ingest_loop(
                            Box::new(source),
                            ingest_store.clone(),
                            ingest_filesystem.clone(),
                            ingest_mount.clone(),
                            ingest_cancel.clone(),
                            classifier_cache.clone(),
                        )
                        .await;
                        if let changelog::IngestExit::RetentionGap(reason) = exit {
                            if let Err(error) = ingest_store
                                .set_baseline_state(
                                    &ingest_filesystem,
                                    rbh_entry_store::model::BaselineState::Invalid,
                                    None,
                                    Some(&reason),
                                )
                                .await
                            {
                                tracing::error!(filesystem = %ingest_filesystem, %error, "failed to invalidate JuiceFS baseline");
                            }
                            tracing::error!(filesystem = %ingest_filesystem, backend = ?backend, %reason, "JuiceFS baseline invalid; restarting with rescan");
                            continue;
                        }
                        if let changelog::IngestExit::BaselineInvalid(reason) = exit {
                            tracing::error!(filesystem = %ingest_filesystem, backend = ?backend, %reason, "JuiceFS baseline comparison failed; restarting with rescan");
                            continue;
                        }
                    }
                    Err(rbh_change_source::ChangeSourceError::RetentionGap(reason)) => {
                        if let Err(error) = ingest_store
                            .set_baseline_state(
                                &ingest_filesystem,
                                rbh_entry_store::model::BaselineState::Invalid,
                                None,
                                Some(&reason),
                            )
                            .await
                        {
                            tracing::error!(filesystem = %ingest_filesystem, %error, "failed to invalidate JuiceFS baseline");
                        }
                        tracing::error!(filesystem = %ingest_filesystem, backend = ?backend, %reason, "JuiceFS baseline invalid; restarting with rescan");
                        continue;
                    }
                    Err(error) => {
                        tracing::error!(filesystem = %ingest_filesystem, backend = ?backend, %endpoint, %error, "JuiceFS Agent unavailable; retrying")
                    }
                }
                tokio::select! {
                    _ = ingest_cancel.cancelled() => break,
                    _ = tokio::time::sleep(std::time::Duration::from_secs(1)) => {}
                }
            }
        });
    }

    // Retain old schedule history and run the wakeup in each daemon.
    let persisted_schedules = MysqlStore::new(pool.clone())
        .await
        .context("failed to initialize scheduler store")?;
    retire_persisted_schedules(&persisted_schedules).await?;

    // Rev.8 policies, due times and execution state live in the catalog.
    // An in-memory trigger avoids sharing scheduler rows with daemons
    // that have no trigger registered for those rows.
    let scheduler = Scheduler::builder()
        .data_store(Arc::new(MemoryStore::new()))
        .executor(Arc::new(scheduler_rs::executor::TokioExecutor::new()))
        .poll_interval(std::time::Duration::from_secs(1))
        .execution_retention(chrono::Duration::minutes(5))
        .build()
        .context("failed to build scheduler")?;

    // Register task types.
    scheduler.register::<rbh_policy::Rev8TickTask>().await;

    // Initialize the Rev.8 worker runtime.
    let (worker_failure, mut worker_failures) = tokio::sync::mpsc::channel(1);
    rbh_policy::init_runtime(Arc::new(rbh_policy::PolicyRuntime {
        policy_store: policy_store.clone(),
        entry_store: entry_store.clone(),
        worker_failure,
    }));

    register_rev8_wakeup(&scheduler)
        .await
        .context("register Rev.8 scheduler wakeup")?;

    // Active HSM state poller. Walks the catalog in small batches,
    // calls llapi_hsm_state_get, patches sm_status if the stored state
    // diverged from MDS truth. Disabled when RBH_HSM_POLL_SECS=0.
    let hsm_poll_secs = std::env::var("RBH_HSM_POLL_SECS")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or(0);
    let hsm_batch = std::env::var("RBH_HSM_POLL_BATCH")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .filter(|n| *n > 0)
        .unwrap_or(200);
    let hsm_pause_ms = std::env::var("RBH_HSM_POLL_PAUSE_MS")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or(500);
    if hsm_poll_secs == 0 {
        tracing::info!("hsm poller disabled (RBH_HSM_POLL_SECS=0)");
    }
    for runtime in runtime_registry.iter() {
        if runtime.should_start_hsm_poller(hsm_poll_secs) {
            let poller = hsm_poller::HsmPoller {
                entry_store: entry_store.clone(),
                lustre: lustre_api::LustreApi,
                filesystem_id: runtime.config.id.clone(),
                mount_path: runtime.config.mount_path.clone(),
                tick: std::time::Duration::from_secs(hsm_poll_secs),
                batch: hsm_batch,
                pause_between_batches: std::time::Duration::from_millis(hsm_pause_ms),
                cancel: daemon_cancel.clone(),
            };
            tokio::spawn(poller.run());
            tracing::info!(
                filesystem = %runtime.config.id,
                backend = ?runtime.config.backend,
                tick_secs = hsm_poll_secs,
                batch = hsm_batch,
                pause_ms = hsm_pause_ms,
                "hsm poller spawned"
            );
        } else if hsm_poll_secs > 0 {
            tracing::info!(filesystem = %runtime.config.id, backend = ?runtime.config.backend, "hsm poller disabled by backend capabilities");
        }
    }

    // 7. Build router with scheduler for trigger reconciliation.
    let state = rbh_api::AppState {
        policy_store,
        classifier_store,
        classifier_cache: classifier_cache.clone(),
        entry_store,
        scans: Arc::new(tokio::sync::Mutex::new(std::collections::HashMap::new())),
    };
    let console_dir = std::env::var_os("RBH_CONSOLE_DIR")
        .filter(|path| !path.is_empty())
        .map(std::path::PathBuf::from)
        .filter(|path| {
            if path.is_absolute() {
                true
            } else {
                tracing::warn!("RBH_CONSOLE_DIR must be absolute; using the packaged console directory");
                false
            }
        })
        .unwrap_or_else(|| "/usr/share/robinhood/console".into());
    let observer_id = match std::env::var("RBH_CONSOLE_OBSERVER_ID") {
        Ok(value) => value,
        Err(std::env::VarError::NotPresent) => std::fs::read_to_string("/proc/sys/kernel/hostname")
            .context("read hostname for Lustre resource observations")?,
        Err(error) => return Err(error).context("read RBH_CONSOLE_OBSERVER_ID"),
    };
    let observer_id = observer_id.trim();
    anyhow::ensure!(
        !observer_id.is_empty() && observer_id.len() <= 255,
        "RBH_CONSOLE_OBSERVER_ID must contain 1 to 255 bytes"
    );
    let console_resources = rbh_lustre_resources::ResourceObservations::new(
        pool.clone(),
        observer_id.to_owned(),
        runtime_registry.iter().map(|runtime| runtime.config.clone()).collect(),
    );
    let console_collect_secs = match std::env::var("RBH_CONSOLE_COLLECT_SECS") {
        Ok(value) => value
            .parse::<u32>()
            .context("RBH_CONSOLE_COLLECT_SECS must be a positive integer")?,
        Err(std::env::VarError::NotPresent) => 60,
        Err(error) => return Err(error).context("read RBH_CONSOLE_COLLECT_SECS"),
    };
    anyhow::ensure!(
        console_collect_secs > 0,
        "RBH_CONSOLE_COLLECT_SECS must be greater than zero"
    );
    let app = rbh_api::router_with_console(state, writers, console_resources.clone(), console_dir);

    // 8. Start HTTP server with graceful shutdown on `daemon_cancel`.
    let listen_addr = std::env::var("RBH_LISTEN_ADDR").unwrap_or_else(|_| "0.0.0.0:8080".to_string());
    let listener = tokio::net::TcpListener::bind(&listen_addr)
        .await
        .with_context(|| format!("failed to bind {listen_addr}"))?;
    resource_collection::register(
        &scheduler,
        console_resources.clone(),
        std::time::Duration::from_secs(console_collect_secs.into()),
    )
    .await
    .context("register Lustre resource collection")?;
    tracing::info!(addr = %listen_addr, "HTTP server listening");

    let shutdown_cancel = daemon_cancel.clone();
    let server_task = tokio::spawn(async move {
        axum::serve(listener, app)
            .with_graceful_shutdown(async move { shutdown_cancel.cancelled().await })
            .await
    });

    // sd_notify(READY=1) — no-op outside systemd, used by `Type=notify`
    // units so the unit blocks in `activating` until the HTTP bind
    // succeeds. `MAINPID=self` lets systemd follow restarts of the
    // child (we don't fork, but documenting the behavior anyway).
    if let Err(e) = sd_notify::notify(false, &[sd_notify::NotifyState::Ready]) {
        tracing::debug!(error = %e, "sd_notify READY failed (likely not under systemd)");
    }

    // 9. Stop serving if the scheduler can no longer process queued work.
    let scheduler_run = scheduler.start();
    tokio::pin!(scheduler_run);
    tracing::info!("scheduler started");
    let outcome = tokio::select! {
        signal = signals::supervise(obs_guard, daemon_cancel.clone(), None, None) => {
            console_resources.stop();
            scheduler.shutdown();
            signal.and(scheduler_run.await.context("Rev.8 scheduler shutdown failed"))
        }
        Some(error) = worker_failures.recv() => {
            daemon_cancel.cancel();
            console_resources.stop();
            scheduler.shutdown();
            if let Err(shutdown_error) = scheduler_run.await {
                tracing::warn!(%shutdown_error, "Rev.8 scheduler shutdown failed");
            }
            Err(error).context("Rev.8 scheduler stopped")
        }
        result = &mut scheduler_run => {
            result
                .and_then(|()| Err(SchedulerError::ExecutionError("unexpected scheduler exit".into())))
                .context("Rev.8 scheduler stopped")
        }
    };
    daemon_cancel.cancel();
    console_resources.stop();

    // Tell systemd we're stopping so it can update unit state before
    // the process exits and suppresses its own timeout warnings.
    let _ = sd_notify::notify(false, &[sd_notify::NotifyState::Stopping]);

    // Await HTTP server drain.
    match server_task.await {
        Ok(Ok(())) => tracing::info!("HTTP server stopped cleanly"),
        Ok(Err(e)) => tracing::warn!(error = %e, "HTTP server error on shutdown"),
        Err(e) => tracing::warn!(error = %e, "HTTP server task panicked"),
    }

    tracing::info!("robinhood-rs daemon stopped");
    outcome
}

/// External legacy configuration compatibility for a sole Lustre runtime.
fn pair_mdts_with_users(mdts_csv: Option<&str>, legacy_mdt: Option<&str>, user_csv: &str) -> Vec<(String, String)> {
    if user_csv.is_empty() {
        return Vec::new();
    }

    let mdt_list: Vec<String> = match mdts_csv.filter(|s| !s.is_empty()) {
        Some(s) => s
            .split(',')
            .map(str::trim)
            .filter(|t| !t.is_empty())
            .map(str::to_string)
            .collect(),
        None => legacy_mdt
            .filter(|s| !s.is_empty())
            .map(|s| vec![s.to_string()])
            .unwrap_or_default(),
    };
    if mdt_list.is_empty() {
        return Vec::new();
    }

    let users: Vec<String> = user_csv
        .split(',')
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .map(str::to_string)
        .collect();
    if users.is_empty() {
        return Vec::new();
    }

    match users.len() {
        1 => mdt_list.into_iter().map(|m| (m, users[0].clone())).collect(),
        n if n == mdt_list.len() => mdt_list.into_iter().zip(users).collect(),
        _ => {
            tracing::warn!(
                mdts = mdt_list.len(),
                users = users.len(),
                "RBH_CHANGELOG_USER count does not match RBH_MDTS (and is not 1); \
                 reusing the first user id across all MDTs"
            );
            let first = users.into_iter().next().unwrap_or_default();
            mdt_list.into_iter().map(|m| (m, first.clone())).collect()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::pair_mdts_with_users;

    #[test]
    fn nothing_configured_is_empty() {
        assert!(pair_mdts_with_users(None, None, "").is_empty());
    }

    #[test]
    fn legacy_single_mdt() {
        let v = pair_mdts_with_users(None, Some("fs-MDT0000"), "cl1");
        assert_eq!(v, vec![("fs-MDT0000".into(), "cl1".into())]);
    }

    #[test]
    fn shared_user_across_mdts() {
        let v = pair_mdts_with_users(Some("a,b,c"), None, "cl9");
        assert_eq!(
            v,
            vec![
                ("a".into(), "cl9".into()),
                ("b".into(), "cl9".into()),
                ("c".into(), "cl9".into()),
            ]
        );
    }

    #[test]
    fn one_user_per_mdt() {
        let v = pair_mdts_with_users(Some("a,b"), None, "cl1,cl2");
        assert_eq!(v, vec![("a".into(), "cl1".into()), ("b".into(), "cl2".into())]);
    }

    #[test]
    fn mismatch_falls_back_to_first_user() {
        let v = pair_mdts_with_users(Some("a,b,c"), None, "cl1,cl2");
        assert_eq!(
            v,
            vec![
                ("a".into(), "cl1".into()),
                ("b".into(), "cl1".into()),
                ("c".into(), "cl1".into()),
            ]
        );
    }

    #[test]
    fn mdts_takes_precedence_over_legacy() {
        let v = pair_mdts_with_users(Some("a,b"), Some("legacy"), "cl1");
        assert_eq!(v, vec![("a".into(), "cl1".into()), ("b".into(), "cl1".into())]);
    }

    #[test]
    fn empty_tokens_are_ignored() {
        let v = pair_mdts_with_users(Some(" a , , b "), None, "cl1, ,cl2");
        assert_eq!(v, vec![("a".into(), "cl1".into()), ("b".into(), "cl2".into())]);
    }
}

/// Drain an fs-scan into the entry store.
async fn run_initial_scan(
    entry_store: &rbh_entry_store::store::EntryStore, filesystem_id: &rbh_entry_store::FileSystemId,
    mount_path: &std::path::Path,
) -> anyhow::Result<()> {
    let config = rbh_fs_scan::ScanConfig {
        root: mount_path.to_path_buf(),
        concurrency: 4,
        max_depth: None,
        channel_size: 1024,
        since_mtime: None,
        ignore_globs: Vec::new(),
    };
    let (mut rx, progress) = rbh_fs_scan::FsScanner::run(config);

    let mut batch: Vec<rbh_entry_store::model::EntryRow> = Vec::with_capacity(100);
    while let Some(event) = rx.recv().await {
        match event {
            rbh_fs_scan::ScanEvent::Entry(entry) => {
                batch.push(*entry);
                if batch.len() >= 100 {
                    persist_scan_batch(entry_store, filesystem_id, &batch).await?;
                    batch.clear();
                }
            }
            rbh_fs_scan::ScanEvent::Error { path, error } => {
                tracing::debug!(path = %path, error = %error, "scan error");
            }
        }
    }
    // Flush remaining.
    if !batch.is_empty() {
        persist_scan_batch(entry_store, filesystem_id, &batch).await?;
    }

    let (scanned, errors, dirs) = progress.snapshot();
    tracing::info!(filesystem = %filesystem_id, scanned, errors, dirs, "fs-scan complete");
    if errors > 0 {
        anyhow::bail!("filesystem scan reported {errors} errors");
    }
    Ok(())
}

async fn persist_scan_batch(
    entry_store: &rbh_entry_store::store::EntryStore, filesystem_id: &rbh_entry_store::FileSystemId,
    entries: &[rbh_entry_store::model::EntryRow],
) -> Result<(), rbh_entry_store::StoreError> {
    entry_store.upsert_lustre_scan_batch(filesystem_id, entries).await
}

async fn run_juicefs_baseline(
    store: &rbh_entry_store::store::EntryStore, filesystem: &rbh_entry_store::FileSystemId, mount: &std::path::Path,
) -> anyhow::Result<()> {
    use rbh_entry_store::model::BaselineState;
    store
        .set_baseline_state(filesystem, BaselineState::Scanning, None, None)
        .await?;
    store.clear_scoped_catalog(filesystem).await?;
    let config = rbh_fs_scan::ScanConfig {
        root: mount.to_path_buf(),
        concurrency: 4,
        max_depth: None,
        channel_size: 1024,
        since_mtime: None,
        ignore_globs: Vec::new(),
    };
    let (mut events, progress) = rbh_fs_scan::PosixWalker::run(config);
    let observed_at = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64;
    while let Some(event) = events.recv().await {
        match event {
            rbh_fs_scan::PosixWalkEvent::Entry(entry) => {
                let (row, edge) =
                    rbh_fs_scan::juicefs::adapt(filesystem, &entry, observed_at).map_err(anyhow::Error::msg)?;
                store.upsert_scoped_entry(&row).await?;
                if let Some(edge) = edge {
                    store.upsert_scoped_namespace_edge(&edge).await?;
                }
            }
            rbh_fs_scan::PosixWalkEvent::Error { path, error } => {
                store
                    .set_baseline_state(filesystem, BaselineState::Invalid, None, Some(&error))
                    .await?;
                anyhow::bail!("JuiceFS baseline scan failed at {path}: {error}");
            }
        }
    }
    let (scanned, errors, dirs) = progress.snapshot();
    if errors != 0 {
        store
            .set_baseline_state(filesystem, BaselineState::Invalid, None, Some("POSIX scan errors"))
            .await?;
        anyhow::bail!("JuiceFS baseline scan completed with {errors} errors");
    }
    store
        .set_baseline_state(filesystem, BaselineState::CatchingUp, None, None)
        .await?;
    tracing::info!(filesystem = %filesystem, scanned, dirs, "JuiceFS baseline scanned; changelog catch-up starting");
    Ok(())
}

/// Pause obsolete tasks before polling the queue, retaining their execution history.
async fn retire_persisted_schedules(store: &MysqlStore) -> Result<(), SchedulerError> {
    for mut schedule in store.list_schedules().await? {
        if matches!(
            schedule.type_name.as_str(),
            "rbh.policy_run" | rbh_policy::Rev8TickTask::TYPE_NAME
        ) && schedule.state == ScheduleState::Active
        {
            schedule.state = ScheduleState::Paused;
            schedule.updated_at = chrono::Utc::now();
            store.update_schedule(&schedule).await?;
        }
    }
    Ok(())
}

async fn register_rev8_wakeup(scheduler: &Scheduler) -> Result<(), SchedulerError> {
    use scheduler_rs::trigger::IntervalTrigger;

    let config = ScheduleConfig {
        misfire_policy: MisfirePolicy::Coalesce,
        max_instances: 1,
        ..Default::default()
    };
    scheduler
        .add_raw(
            rbh_policy::Rev8TickTask::TYPE_NAME.to_owned(),
            serde_json::Value::Null,
            Box::new(IntervalTrigger::every(rbh_policy::Rev8TickTask::INTERVAL)),
            config,
            Some("rbh.rev8.tick".into()),
        )
        .await?;
    Ok(())
}
