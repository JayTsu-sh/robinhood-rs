//! robinhood-rs daemon library.
//!
//! The public [`run`] function boots the full stack: observability, database,
//! changelog listener, fs-scan, scheduler-rs, and the axum HTTP server.

#![allow(clippy::items_after_test_module)]

mod changelog;
mod hsm_poller;
mod runtime;
mod signals;
mod thresholds;

use std::sync::Arc;

use anyhow::Context;
use scheduler_rs::prelude::*;
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
    let policy_store = rbh_policy::PolicyStore::new(pool.clone());
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
        let rebound = policy_store
            .bind_legacy_lustre_filesystem(&runtime.config.id)
            .await
            .context("failed to bind legacy policies to the sole configured Lustre filesystem")?;
        if rebound > 0 {
            tracing::info!(filesystem = %runtime.config.id, policies = rebound, "legacy policies bound to sole Lustre filesystem");
        }
    } else if !lustre.is_empty() {
        tracing::warn!(
            lustre_filesystems = lustre.len(),
            "legacy unscoped policies and changelog cursors are not rebound when multiple Lustre filesystems are configured"
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

    // 6. Set up scheduler-rs.
    let scheduler_store = Arc::new(
        scheduler_rs::store::MysqlStore::new(pool.clone())
            .await
            .context("failed to initialize scheduler store")?,
    );
    let scheduler = Scheduler::builder()
        .data_store(scheduler_store)
        .executor(Arc::new(scheduler_rs::executor::TokioExecutor::new()))
        .poll_interval(std::time::Duration::from_secs(1))
        .build()
        .context("failed to build scheduler")?;

    // Register task types.
    scheduler.register::<rbh_policy::PolicyRunTask>().await;

    // Initialize the policy runtime (global state for PolicyRunTask).
    rbh_policy::init_runtime(Arc::new(rbh_policy::PolicyRuntime {
        policy_store: policy_store.clone(),
        entry_store: entry_store.clone(),
    }));

    // Reconcile existing policies → scheduler schedules.
    reconcile_all_policies(&scheduler, &policy_store, &entry_store).await;

    // Prune threshold-fire schedules left behind by previous runs. Their
    // names follow `rbh.policy.<id>.threshold.<idx>.<unix>` and the
    // scheduler marks them Completed but never removes them — this
    // keeps the schedules table from growing unbounded.
    prune_threshold_schedules(&scheduler).await;

    // Start the scheduler loop.
    let _scheduler_handle = scheduler.spawn();
    tracing::info!("scheduler started");

    // Periodic pruner (every 10 minutes) for long-lived daemons that
    // fire thousands of threshold events.
    {
        let scheduler_for_prune = scheduler.clone();
        let cancel_for_prune = daemon_cancel.clone();
        tokio::spawn(async move {
            let interval = std::time::Duration::from_secs(600);
            loop {
                tokio::select! {
                    _ = cancel_for_prune.cancelled() => return,
                    _ = tokio::time::sleep(interval) => {}
                }
                prune_threshold_schedules(&scheduler_for_prune).await;
            }
        });
    }

    // Threshold checker — polls enabled policies for ThresholdCount /
    // ThresholdVolume triggers and fires immediate policy runs on hit.
    let threshold_tick_secs = std::env::var("RBH_THRESHOLD_TICK_SECS")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .filter(|s| *s > 0)
        .unwrap_or(30);
    for runtime in runtime_registry.iter() {
        let checker = thresholds::ThresholdChecker {
            policy_store: policy_store.clone(),
            entry_store: entry_store.clone(),
            scheduler: scheduler.clone(),
            lustre: lustre_api::LustreApi,
            filesystem_id: runtime.config.id.clone(),
            backend: runtime.config.backend,
            mount_path: runtime.config.mount_path.clone(),
            tick: std::time::Duration::from_secs(threshold_tick_secs),
            cancel: daemon_cancel.clone(),
        };
        tokio::spawn(checker.run());
        tracing::info!(filesystem = %runtime.config.id, tick_secs = threshold_tick_secs, "threshold checker spawned");
    }

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
        scheduler: Some(scheduler.clone()),
        scans: Arc::new(tokio::sync::Mutex::new(std::collections::HashMap::new())),
    };
    let app = rbh_api::router(state);

    // 8. Start HTTP server with graceful shutdown on `daemon_cancel`.
    let listen_addr = std::env::var("RBH_LISTEN_ADDR").unwrap_or_else(|_| "0.0.0.0:8080".to_string());
    let listener = tokio::net::TcpListener::bind(&listen_addr)
        .await
        .with_context(|| format!("failed to bind {listen_addr}"))?;
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

    // 9. Signal supervisor. Blocks until SIGTERM/SIGINT, then flips cancel.
    signals::supervise(obs_guard, daemon_cancel.clone(), None, None).await?;

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
    Ok(())
}

/// Remove Completed one-shot policy schedules from the scheduler DB.
///
/// Both threshold fires and manual `POST /api/policies/:id/run` calls
/// produce ImmediateTrigger schedules named `rbh.policy.<id>.threshold.*`
/// or `rbh.policy.<id>.manual.*`. scheduler-rs marks them Completed
/// after the one-shot runs but never removes them. This sweep drops
/// Completed rows matching either prefix.
async fn prune_threshold_schedules(scheduler: &Scheduler) {
    let records = match scheduler.list_schedules_by_name_prefix("rbh.policy.").await {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!(error = %e, "prune: list_schedules_by_name_prefix failed");
            return;
        }
    };
    let mut pruned = 0u64;
    for rec in records {
        let name = match &rec.name {
            Some(n) => n,
            None => continue,
        };
        if !name.contains(".threshold.") && !name.contains(".manual.") {
            continue;
        }
        if !matches!(rec.state, scheduler_rs::prelude::ScheduleState::Completed) {
            continue;
        }
        if let Err(e) = scheduler.remove(&rec.id).await {
            tracing::warn!(name = %name, error = %e, "prune: remove failed");
            continue;
        }
        pruned += 1;
    }
    if pruned > 0 {
        tracing::info!(pruned, "pruned completed one-shot schedules");
    }
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

/// Reconcile all enabled policies to scheduler-rs schedules on startup.
async fn reconcile_all_policies(
    scheduler: &Scheduler, policy_store: &rbh_policy::PolicyStore, entry_store: &rbh_entry_store::store::EntryStore,
) {
    match policy_store.list().await {
        Ok(policies) => {
            for policy in &policies {
                if policy.enabled {
                    let validation = match entry_store.get_filesystem(&policy.definition.filesystem).await {
                        Ok(Some(config)) => rbh_policy::validate_policy_for_filesystem(&policy.definition, &config),
                        Ok(None) => Err(rbh_policy::PolicyError::Store(format!(
                            "unknown filesystem: {}",
                            policy.definition.filesystem
                        ))),
                        Err(error) => Err(rbh_policy::PolicyError::Store(error.to_string())),
                    };
                    if let Err(error) = validation {
                        tracing::error!(policy_id = policy.id, %error, "policy rejected before schedule reconciliation");
                        if let Err(remove_error) =
                            rbh_policy::reconcile::remove_policy_schedule(scheduler, policy.id).await
                        {
                            tracing::error!(
                                policy_id = policy.id,
                                error = %remove_error,
                                "failed to remove schedule for rejected policy"
                            );
                        }
                        continue;
                    }
                    match rbh_policy::reconcile_triggers(
                        scheduler,
                        policy.id,
                        &policy.definition.trigger,
                        policy.definition.enabled,
                    )
                    .await
                    {
                        Ok(ids) => {
                            tracing::info!(policy_id = policy.id, scheduled = ids.is_some(), "policy reconciled");
                        }
                        Err(e) => {
                            tracing::error!(
                                policy_id = policy.id,
                                error = %e,
                                "failed to reconcile policy"
                            );
                        }
                    }
                }
            }
        }
        Err(e) => {
            tracing::error!(error = %e, "failed to list policies for reconciliation");
        }
    }
}
