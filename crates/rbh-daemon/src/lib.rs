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
    }
    let lustre_runtime = runtime_registry.lustre().clone();
    let filesystem_id = lustre_runtime.config.id.clone();
    let mount_path = lustre_runtime.config.mount_path.clone();
    let capabilities = lustre_runtime.config.capabilities;
    tracing::info!(
        filesystem = %filesystem_id,
        backend = ?lustre_runtime.config.backend,
        mount = %mount_path.display(),
        ?capabilities,
        "filesystem runtime selected"
    );
    let policy_store = rbh_policy::PolicyStore::new(pool.clone());
    policy_store
        .check_schema()
        .await
        .context("policy schema check failed")?;
    let classifier_store = rbh_policy::ClassifierStore::new(pool.clone());

    // Load classifier cache for changelog-driven classification.
    let classifier_cache: std::sync::Arc<tokio::sync::RwLock<Vec<rbh_policy::ClassifierRow>>> = std::sync::Arc::new(
        tokio::sync::RwLock::new(classifier_store.list().await.unwrap_or_default()),
    );
    tracing::info!("database migrations complete");

    // 4. Initial fs-scan if catalog is empty.
    let count = entry_store.entry_count().await.unwrap_or(0);
    if count == 0 {
        tracing::info!(filesystem = %filesystem_id, mount = %mount_path.display(), "catalog empty — running initial fs-scan");
        run_initial_scan(&entry_store, &filesystem_id, &mount_path).await;
        let new_count = entry_store.entry_count().await.unwrap_or(0);
        tracing::info!(filesystem = %filesystem_id, entries = new_count, "initial scan complete");
    } else {
        tracing::info!(filesystem = %filesystem_id, entries = count, "catalog already populated");
    }

    // 5. Spawn one changelog listener per configured MDT.
    //
    // Env:
    //   RBH_MDTS             — comma-separated MDT names (e.g. "fs-MDT0000,fs-MDT0001")
    //                          Falls back to RBH_MDT_NAME for backward compat.
    //   RBH_CHANGELOG_USER   — either a single reader id reused on every MDT,
    //                          or a comma-separated list matching RBH_MDTS 1:1.
    let daemon_cancel = CancellationToken::new();
    let cursor_store = Arc::new(rbh_entry_store::store::MariaDbCursorStore::new(pool.clone()));

    let mdt_specs = resolve_changelog_mdts();
    if mdt_specs.is_empty() {
        tracing::info!("RBH_MDTS / RBH_CHANGELOG_USER not set — changelog listener disabled");
    } else {
        for (mdt_name, reader_id) in mdt_specs {
            let listener_cfg = lustre_changelog::ListenerConfig {
                mdt: mdt_name.clone(),
                reader_id: reader_id.clone(),
                follow: true,
                channel_buffer: 32,
                ..Default::default()
            };

            match lustre_changelog::ChangelogListener::spawn(listener_cfg, cursor_store.clone(), daemon_cancel.clone())
                .await
            {
                Ok(handle) => {
                    tracing::info!(
                        filesystem = %filesystem_id,
                        mdt = %mdt_name,
                        reader_id = %reader_id,
                        "changelog listener started"
                    );
                    let ingest_store = entry_store.clone();
                    let ingest_mount = mount_path.clone();
                    let ingest_filesystem = filesystem_id.clone();
                    let ingest_cancel = daemon_cancel.clone();
                    let classifier_cache_cl = classifier_cache.clone();
                    let source = rbh_change_source::LustreChangeSource::new(ingest_filesystem.clone(), handle);
                    tokio::spawn(async move {
                        changelog::ingest_loop(
                            Box::new(source),
                            ingest_store,
                            ingest_filesystem,
                            ingest_mount,
                            ingest_cancel,
                            classifier_cache_cl.clone(),
                        )
                        .await;
                    });
                }
                Err(e) => {
                    // Isolate per-MDT failures — other MDTs keep running.
                    tracing::error!(
                        filesystem = %filesystem_id,
                        mdt = %mdt_name,
                        reader_id = %reader_id,
                        error = %e,
                        "failed to start changelog listener for this MDT \
                         — continuing without it"
                    );
                }
            }
        }
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
        filesystem_id: filesystem_id.clone(),
        mount_path: mount_path.clone(),
    }));

    // Reconcile existing policies → scheduler schedules.
    reconcile_all_policies(&scheduler, &policy_store).await;

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
    let checker = thresholds::ThresholdChecker {
        policy_store: policy_store.clone(),
        entry_store: entry_store.clone(),
        scheduler: scheduler.clone(),
        lustre: lustre_api::LustreApi,
        filesystem_id: filesystem_id.clone(),
        mount_path: mount_path.clone(),
        tick: std::time::Duration::from_secs(threshold_tick_secs),
        cancel: daemon_cancel.clone(),
    };
    tokio::spawn(checker.run());
    tracing::info!(filesystem = %filesystem_id, tick_secs = threshold_tick_secs, "threshold checker spawned");

    // Active HSM state poller. Walks the catalog in small batches,
    // calls llapi_hsm_state_get, patches sm_status if the stored state
    // diverged from MDS truth. Disabled when RBH_HSM_POLL_SECS=0.
    let hsm_poll_secs = std::env::var("RBH_HSM_POLL_SECS")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or(0);
    if lustre_runtime.should_start_hsm_poller(hsm_poll_secs) {
        let hsm_batch = std::env::var("RBH_HSM_POLL_BATCH")
            .ok()
            .and_then(|s| s.parse::<u64>().ok())
            .filter(|n| *n > 0)
            .unwrap_or(200);
        let hsm_pause_ms = std::env::var("RBH_HSM_POLL_PAUSE_MS")
            .ok()
            .and_then(|s| s.parse::<u64>().ok())
            .unwrap_or(500);
        let poller = hsm_poller::HsmPoller {
            entry_store: entry_store.clone(),
            lustre: lustre_api::LustreApi,
            filesystem_id: filesystem_id.clone(),
            mount_path: mount_path.clone(),
            tick: std::time::Duration::from_secs(hsm_poll_secs),
            batch: hsm_batch,
            pause_between_batches: std::time::Duration::from_millis(hsm_pause_ms),
            cancel: daemon_cancel.clone(),
        };
        tokio::spawn(poller.run());
        tracing::info!(
            filesystem = %filesystem_id,
            tick_secs = hsm_poll_secs,
            batch = hsm_batch,
            pause_ms = hsm_pause_ms,
            "hsm poller spawned"
        );
    } else if hsm_poll_secs == 0 {
        tracing::info!("hsm poller disabled (RBH_HSM_POLL_SECS=0)");
    } else {
        tracing::info!(filesystem = %filesystem_id, "hsm poller disabled by backend capabilities");
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

/// Resolve the list of (MDT, changelog reader id) pairs from environment.
fn resolve_changelog_mdts() -> Vec<(String, String)> {
    let mdts_raw = std::env::var("RBH_MDTS").ok();
    let legacy_mdt = std::env::var("RBH_MDT_NAME").ok();
    let user_raw = std::env::var("RBH_CHANGELOG_USER").unwrap_or_default();
    pair_mdts_with_users(mdts_raw.as_deref(), legacy_mdt.as_deref(), &user_raw)
}

/// Pure resolver used by [`resolve_changelog_mdts`]. Pulled out for testing.
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
) {
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
                    if let Err(e) = persist_scan_batch(entry_store, filesystem_id, &batch).await {
                        tracing::warn!(error = %e, "batch upsert failed");
                    }
                    batch.clear();
                }
            }
            rbh_fs_scan::ScanEvent::Error { path, error } => {
                tracing::debug!(path = %path, error = %error, "scan error");
            }
        }
    }
    // Flush remaining.
    if !batch.is_empty()
        && let Err(e) = persist_scan_batch(entry_store, filesystem_id, &batch).await
    {
        tracing::warn!(error = %e, "final batch upsert failed");
    }

    let (scanned, errors, dirs) = progress.snapshot();
    tracing::info!(filesystem = %filesystem_id, scanned, errors, dirs, "fs-scan complete");
}

async fn persist_scan_batch(
    entry_store: &rbh_entry_store::store::EntryStore, filesystem_id: &rbh_entry_store::FileSystemId,
    entries: &[rbh_entry_store::model::EntryRow],
) -> Result<(), rbh_entry_store::StoreError> {
    entry_store.upsert_lustre_scan_batch(filesystem_id, entries).await
}

/// Reconcile all enabled policies to scheduler-rs schedules on startup.
async fn reconcile_all_policies(scheduler: &Scheduler, policy_store: &rbh_policy::PolicyStore) {
    match policy_store.list().await {
        Ok(policies) => {
            for policy in &policies {
                if policy.enabled {
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
