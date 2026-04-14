//! robinhood-rs daemon library.
//!
//! The public [`run`] function boots the full stack: observability, database,
//! changelog listener, fs-scan, scheduler-rs, and the axum HTTP server.

mod changelog;

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Context;
use scheduler_rs::prelude::*;
use tokio_util::sync::CancellationToken;

/// Run the robinhood-rs daemon.
pub async fn run() -> anyhow::Result<()> {
    // 1. Observability — must be first.
    let _guard = rbh_observability::init(rbh_observability::ObservabilityConfig {
        service_name: "rbh-daemon",
        ..Default::default()
    })
    .context("failed to initialize observability")?;

    tracing::info!("robinhood-rs daemon starting");

    // 2. Database connection.
    let db_url = std::env::var("RBH_DATABASE_URL")
        .unwrap_or_else(|_| "mysql://root@127.0.0.1/rbh_entries".to_string());
    let pool = sqlx::MySqlPool::connect(&db_url)
        .await
        .context("failed to connect to MariaDB")?;

    // 3. Run migrations (entry-store owns all schema).
    let entry_store = rbh_entry_store::store::EntryStore::connect(&db_url)
        .await
        .context("failed to initialize entry store")?;
    let policy_store = rbh_policy::PolicyStore::new(pool.clone());
    policy_store
        .check_schema()
        .await
        .context("policy schema check failed")?;
    tracing::info!("database migrations complete");

    // 4. Initial fs-scan if catalog is empty.
    let mount_path = std::env::var("RBH_LUSTRE_MOUNT")
        .unwrap_or_else(|_| "/lustre".to_string());
    let count = entry_store.entry_count().await.unwrap_or(0);
    if count == 0 {
        tracing::info!(mount = %mount_path, "catalog empty — running initial fs-scan");
        run_initial_scan(&entry_store, &mount_path).await;
        let new_count = entry_store.entry_count().await.unwrap_or(0);
        tracing::info!(entries = new_count, "initial scan complete");
    } else {
        tracing::info!(entries = count, "catalog already populated");
    }

    // 5. Spawn changelog listener(s) for continuous catalog updates.
    let daemon_cancel = CancellationToken::new();
    let changelog_reader_id = std::env::var("RBH_CHANGELOG_USER")
        .unwrap_or_else(|_| String::new());

    if !changelog_reader_id.is_empty() {
        let mdt_name = std::env::var("RBH_MDT_NAME")
            .unwrap_or_else(|_| "testfs-MDT0000".to_string());

        let cursor_store = Arc::new(
            rbh_entry_store::store::MariaDbCursorStore::new(pool.clone()),
        );

        let listener_cfg = lustre_changelog::ListenerConfig {
            mdt: mdt_name.clone(),
            reader_id: changelog_reader_id.clone(),
            follow: true,
            channel_buffer: 32,
            ..Default::default()
        };

        match lustre_changelog::ChangelogListener::spawn(
            listener_cfg,
            cursor_store,
            daemon_cancel.clone(),
        ).await {
            Ok(handle) => {
                tracing::info!(
                    mdt = %mdt_name,
                    reader_id = %changelog_reader_id,
                    "changelog listener started"
                );

                // Spawn the ingest task that applies events to the entry store.
                let ingest_store = entry_store.clone();
                let ingest_mount = PathBuf::from(&mount_path);
                let ingest_cancel = daemon_cancel.clone();
                tokio::spawn(async move {
                    changelog::ingest_loop(handle, ingest_store, ingest_mount, ingest_cancel).await;
                });
            }
            Err(e) => {
                tracing::error!(
                    mdt = %mdt_name,
                    reader_id = %changelog_reader_id,
                    error = %e,
                    "failed to start changelog listener — running without live updates"
                );
            }
        }
    } else {
        tracing::info!("RBH_CHANGELOG_USER not set — changelog listener disabled");
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
    scheduler
        .register::<rbh_policy::PolicyRunTask>()
        .await;

    // Initialize the policy runtime (global state for PolicyRunTask).
    rbh_policy::init_runtime(Arc::new(rbh_policy::PolicyRuntime {
        policy_store: policy_store.clone(),
        entry_store: entry_store.clone(),
        mount_path: PathBuf::from(&mount_path),
    }));

    // Reconcile existing policies → scheduler schedules.
    reconcile_all_policies(&scheduler, &policy_store).await;

    // Start the scheduler loop.
    let _scheduler_handle = scheduler.spawn();
    tracing::info!("scheduler started");

    // 7. Build router with scheduler for trigger reconciliation.
    let state = rbh_api::AppState {
        policy_store,
        entry_store,
        scheduler: Some(scheduler.clone()),
    };
    let app = rbh_api::router(state);

    // 8. Start HTTP server.
    let listen_addr =
        std::env::var("RBH_LISTEN_ADDR").unwrap_or_else(|_| "0.0.0.0:8080".to_string());
    let listener = tokio::net::TcpListener::bind(&listen_addr)
        .await
        .with_context(|| format!("failed to bind {listen_addr}"))?;
    tracing::info!(addr = %listen_addr, "HTTP server listening");

    axum::serve(listener, app)
        .await
        .context("HTTP server error")?;

    Ok(())
}

/// Drain an fs-scan into the entry store.
async fn run_initial_scan(
    entry_store: &rbh_entry_store::store::EntryStore,
    mount_path: &str,
) {
    let config = rbh_fs_scan::ScanConfig {
        root: PathBuf::from(mount_path),
        concurrency: 4,
        max_depth: None,
        channel_size: 1024,
    };
    let (mut rx, progress) = rbh_fs_scan::FsScanner::run(config);

    let mut batch: Vec<rbh_entry_store::model::EntryRow> = Vec::with_capacity(100);
    while let Some(event) = rx.recv().await {
        match event {
            rbh_fs_scan::ScanEvent::Entry(entry) => {
                batch.push(*entry);
                if batch.len() >= 100 {
                    if let Err(e) = entry_store.upsert_batch(&batch).await {
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
    if !batch.is_empty() {
        if let Err(e) = entry_store.upsert_batch(&batch).await {
            tracing::warn!(error = %e, "final batch upsert failed");
        }
    }

    let (scanned, errors, dirs) = progress.snapshot();
    tracing::info!(scanned, errors, dirs, "fs-scan complete");
}

/// Reconcile all enabled policies to scheduler-rs schedules on startup.
async fn reconcile_all_policies(
    scheduler: &Scheduler,
    policy_store: &rbh_policy::PolicyStore,
) {
    match policy_store.list().await {
        Ok(policies) => {
            for policy in &policies {
                if policy.enabled {
                    match rbh_policy::reconcile_triggers(scheduler, policy.id, &policy.definition)
                        .await
                    {
                        Ok(ids) => {
                            tracing::info!(
                                policy_id = policy.id,
                                schedules = ids.len(),
                                "policy reconciled"
                            );
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
