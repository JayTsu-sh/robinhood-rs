//! The async changelog listener — bridges `lustre-api`'s blocking recv loop
//! into the tokio runtime via `spawn_blocking`.
//!
//! # Data flow
//!
//! ```text
//!   ┌──────── blocking thread ──────────┐     ┌──── async consumer ────┐
//!   │ recv → parse → batcher → flush ──►│──►  │ events.recv()          │
//!   │                                   │     │ apply_batch(...)       │
//!   │ ack_rx.try_recv() ◄───────────────│◄──  │ acks.send(EventAck)   │
//!   │ → clear_changelog(committed_idx)  │     │                       │
//!   └──────────────────────────────────-┘     └────────────────────────┘
//! ```
//!
//! Two bounded mpsc channels cross the blocking↔async boundary.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};

use lustre_api::{ChangelogEventType, LustreApi, RecView};

use crate::batcher::{BatcherConfig, EventBatch, EventBatcher};
use crate::cursor::CursorStore;
use crate::error::ChangelogError;
use crate::parse;

/// Configuration for one [`ChangelogListener`] instance (one per MDT).
#[derive(Debug, Clone)]
pub struct ListenerConfig {
    /// Short MDT name, e.g. `"testfs-MDT0000"`.
    pub mdt: String,
    /// Pre-registered changelog reader id, e.g. `"cl1"`.
    pub reader_id: String,
    /// `true` for continuous operation (CHANGELOG_FLAG_FOLLOW | BLOCK);
    /// `false` to drain existing records then stop.
    pub follow: bool,
    /// When `follow` is true, use drain-sleep-reopen instead of blocking
    /// indefinitely. The listener opens the changelog in non-follow mode,
    /// drains all available records, flushes the batcher, sleeps for
    /// `poll_interval`, then reopens. This ensures:
    /// - Time-based flush triggers fire promptly (no events stuck in batcher)
    /// - Cancellation is responsive (checked every poll_interval)
    /// - No events are lost (cursor tracks last processed position)
    ///
    /// Set to `Duration::ZERO` to use the original blocking follow mode
    /// (only for backwards compatibility / tests).
    pub poll_interval: Duration,
    /// Batcher settings (flush interval, batch size, pending soft cap).
    pub batcher: BatcherConfig,
    /// Bounded channel capacity for `EventBatch` delivery.
    pub channel_buffer: usize,
    /// Event types that are silently dropped at parse time (before dedup).
    pub ignored_types: HashSet<ChangelogEventType>,
}

impl Default for ListenerConfig {
    fn default() -> Self {
        Self {
            mdt: String::new(),
            reader_id: String::new(),
            follow: true,
            poll_interval: Duration::from_secs(1),
            batcher: BatcherConfig::default(),
            channel_buffer: 16,
            ignored_types: [
                ChangelogEventType::Mark,
                ChangelogEventType::Open,
                ChangelogEventType::NoOpen,
                ChangelogEventType::ATime,
            ]
            .into_iter()
            .collect(),
        }
    }
}

/// Acknowledgement sent by the consumer after durably committing a batch.
#[derive(Debug)]
pub struct EventAck {
    pub mdt: String,
    pub committed_index: u64,
}

/// Handle returned by [`ChangelogListener::spawn`]. The consumer reads
/// batches from `events` and sends acks back via `acks`.
pub struct ListenerHandle {
    /// Receive batches of changelog events.
    pub events: mpsc::Receiver<EventBatch>,
    /// Send acknowledgements after durable DB commit.
    pub acks: mpsc::Sender<EventAck>,
    /// Cancel this token to request a graceful shutdown of the listener.
    pub cancel: CancellationToken,
}

/// One-MDT changelog listener. Call [`spawn`](Self::spawn) to start.
pub struct ChangelogListener;

impl ChangelogListener {
    /// Start the listener. Returns a [`ListenerHandle`] for the consumer.
    ///
    /// The listener:
    /// 1. Reads the resume cursor from `cursor_store.get(cfg.mdt)`.
    /// 2. Opens a changelog stream on the MDT via `lustre-api`.
    /// 3. Enters a blocking recv loop (on a `spawn_blocking` thread).
    /// 4. Parses, dedup/batches, and sends `EventBatch` through the channel.
    /// 5. Processes `EventAck`s to drive `clear_changelog` and cursor commits.
    /// 6. On `cancel` or EOF (non-follow mode), flushes the partial batch,
    ///    commits the cursor, and exits.
    pub async fn spawn(
        cfg: ListenerConfig, cursor_store: Arc<dyn CursorStore>, cancel: CancellationToken,
    ) -> Result<ListenerHandle, ChangelogError> {
        let start_rec = cursor_store.get(&cfg.mdt).await?.map(|pos| pos as i64 + 1).unwrap_or(0);

        let (event_tx, event_rx) = mpsc::channel::<EventBatch>(cfg.channel_buffer);
        let (ack_tx, ack_rx) = mpsc::channel::<EventAck>(cfg.channel_buffer);

        let listener_cancel = cancel.clone();
        let mdt = cfg.mdt.clone();
        let reader_id = cfg.reader_id.clone();

        tokio::task::spawn_blocking(move || {
            if let Err(e) = run_blocking(cfg, start_rec, event_tx, ack_rx, cursor_store, listener_cancel) {
                error!(mdt = %mdt, reader_id = %reader_id, "listener exited with error: {e}");
            }
        });

        Ok(ListenerHandle {
            events: event_rx,
            acks: ack_tx,
            cancel,
        })
    }
}

/// The blocking recv loop. Runs on a `spawn_blocking` thread.
///
/// Two modes of operation:
///
/// 1. **Drain-sleep-reopen** (default, `poll_interval > 0`): Opens the changelog
///    in non-follow mode, drains all available records, flushes the batcher,
///    processes acks, sleeps for `poll_interval`, then reopens. This ensures
///    time-based flush triggers fire promptly and cancellation is responsive.
///
/// 2. **Blocking follow** (legacy, `poll_interval == 0`): Opens with
///    `CHANGELOG_FLAG_FOLLOW | BLOCK`, blocking indefinitely on `recv_changelog`.
///    Events may sit in the batcher for an unbounded time on a quiet filesystem,
///    and cancellation requires the next record to arrive.
#[tracing::instrument(
    name = "changelog.listener.loop",
    skip(cfg, event_tx, ack_rx, cursor_store, cancel),
    fields(mdt = %cfg.mdt, reader_id = %cfg.reader_id, start_rec),
)]
fn run_blocking(
    cfg: ListenerConfig, start_rec: i64, event_tx: mpsc::Sender<EventBatch>, ack_rx: mpsc::Receiver<EventAck>,
    cursor_store: Arc<dyn CursorStore>, cancel: CancellationToken,
) -> Result<(), ChangelogError> {
    let use_polling = cfg.follow && cfg.poll_interval > Duration::ZERO;

    if use_polling {
        run_polling_loop(cfg, start_rec, event_tx, ack_rx, cursor_store, cancel)
    } else {
        run_follow_loop(cfg, start_rec, event_tx, ack_rx, cursor_store, cancel)
    }
}

/// Drain-sleep-reopen loop. Opens changelog without follow, drains all records,
/// flushes, sleeps, reopens. Repeat until cancelled.
fn run_polling_loop(
    cfg: ListenerConfig, start_rec: i64, event_tx: mpsc::Sender<EventBatch>, mut ack_rx: mpsc::Receiver<EventAck>,
    cursor_store: Arc<dyn CursorStore>, cancel: CancellationToken,
) -> Result<(), ChangelogError> {
    let api = LustreApi::new();
    let mut batcher = EventBatcher::new(&cfg.mdt, cfg.batcher.clone());
    let mut ack_state = AckState {
        last_cleared: start_rec.saturating_sub(1),
        records_since_commit: 0,
        last_commit_time: std::time::Instant::now(),
    };
    let mut next_start = start_rec;

    info!(mdt = %cfg.mdt, start_rec, poll_ms = cfg.poll_interval.as_millis() as u64, "changelog listener started (polling mode)");

    loop {
        if cancel.is_cancelled() {
            debug!(mdt = %cfg.mdt, "cancellation requested");
            break;
        }

        // Open in non-follow mode — drain all available records then EOF.
        let handle = match api.open_changelog(&cfg.mdt, next_start, false) {
            Ok(h) => h,
            Err(e) => {
                warn!(mdt = %cfg.mdt, err = %e, "failed to open changelog; retrying after sleep");
                std::thread::sleep(cfg.poll_interval);
                continue;
            }
        };

        let mut drained = 0u64;
        loop {
            let buf = match api.recv_changelog(&handle) {
                Ok(Some(buf)) => buf,
                Ok(None) => break, // EOF — all current records drained
                Err(e) => {
                    if matches!(&e, lustre_api::LustreApiError::Ffi { errno, .. } if *errno == libc::EINTR) {
                        continue;
                    }
                    warn!(mdt = %cfg.mdt, err = %e, "recv error during drain");
                    break;
                }
            };

            let view = unsafe { RecView::new(buf.as_ptr()) };
            if cfg.ignored_types.contains(&view.event_type()) {
                continue;
            }

            let envelope = match parse::parse_event(&cfg.mdt, &view) {
                Ok(Some(env)) => env,
                Ok(None) => continue,
                Err(e) => {
                    warn!(mdt = %cfg.mdt, err = %e, "failed to parse record; skipping");
                    continue;
                }
            };

            // Track the highest record index for reopen position.
            next_start = envelope.index as i64 + 1;
            drop(buf);
            drained += 1;

            if batcher.push(envelope)
                && let Some(batch) = batcher.flush()
                && event_tx.blocking_send(batch).is_err()
            {
                let _ = api.close_changelog(handle);
                return Err(ChangelogError::EventChannelClosed);
            }
        }

        // Close the handle before sleeping.
        let _ = api.close_changelog(handle);

        // Flush any remaining events in the batcher (the time-based trigger).
        if let Some(batch) = batcher.flush() {
            let count = batch.events.len();
            if event_tx.blocking_send(batch).is_err() {
                warn!(mdt = %cfg.mdt, dropped_events = count, "consumer gone; partial batch dropped");
                return Err(ChangelogError::EventChannelClosed);
            }
        }

        // Process acks and advance watermarks.
        process_acks(
            &api,
            &cfg.mdt,
            &cfg.reader_id,
            &mut ack_rx,
            &mut ack_state,
            &cursor_store,
        );

        if drained > 0 {
            debug!(mdt = %cfg.mdt, drained, next_start, "drain cycle complete");
        }

        // Sleep before next drain cycle, checking cancellation.
        if !cancel.is_cancelled() {
            std::thread::sleep(cfg.poll_interval);
        }
    }

    // Final flush + ack on shutdown.
    if let Some(batch) = batcher.flush() {
        let count = batch.events.len();
        if event_tx.blocking_send(batch).is_err() {
            warn!(mdt = %cfg.mdt, dropped_events = count, "consumer gone; partial batch dropped on shutdown");
        }
    }
    process_acks(
        &api,
        &cfg.mdt,
        &cfg.reader_id,
        &mut ack_rx,
        &mut ack_state,
        &cursor_store,
    );
    info!(mdt = %cfg.mdt, last_cleared = ack_state.last_cleared, "changelog listener stopped");
    Ok(())
}

/// Original blocking follow loop. Kept for `poll_interval == 0` or `follow: false`.
fn run_follow_loop(
    cfg: ListenerConfig, start_rec: i64, event_tx: mpsc::Sender<EventBatch>, mut ack_rx: mpsc::Receiver<EventAck>,
    cursor_store: Arc<dyn CursorStore>, cancel: CancellationToken,
) -> Result<(), ChangelogError> {
    let api = LustreApi::new();
    let mut batcher = EventBatcher::new(&cfg.mdt, cfg.batcher.clone());
    let mut ack_state = AckState {
        last_cleared: start_rec.saturating_sub(1),
        records_since_commit: 0,
        last_commit_time: std::time::Instant::now(),
    };

    let mut handle = api.open_changelog(&cfg.mdt, start_rec, cfg.follow)?;
    info!(mdt = %cfg.mdt, start_rec, "changelog listener started (follow mode)");

    loop {
        if cancel.is_cancelled() {
            debug!(mdt = %cfg.mdt, "cancellation requested");
            break;
        }

        process_acks(
            &api,
            &cfg.mdt,
            &cfg.reader_id,
            &mut ack_rx,
            &mut ack_state,
            &cursor_store,
        );

        let buf = match api.recv_changelog(&handle) {
            Ok(Some(buf)) => buf,
            Ok(None) => {
                info!(mdt = %cfg.mdt, "changelog EOF reached");
                break;
            }
            Err(e) => {
                if matches!(&e, lustre_api::LustreApiError::Ffi { errno, .. } if *errno == libc::EINTR) {
                    continue;
                }
                if let Some(batch) = batcher.flush() {
                    let count = batch.events.len();
                    if event_tx.blocking_send(batch).is_err() {
                        warn!(mdt = %cfg.mdt, dropped_events = count, "consumer gone during error recovery");
                        return Err(ChangelogError::EventChannelClosed);
                    }
                }
                warn!(mdt = %cfg.mdt, err = %e, "recv error; will retry after sleep");
                drop(handle);
                std::thread::sleep(Duration::from_secs(1));
                handle = api.open_changelog(&cfg.mdt, ack_state.last_cleared + 1, cfg.follow)?;
                continue;
            }
        };

        let view = unsafe { RecView::new(buf.as_ptr()) };
        if cfg.ignored_types.contains(&view.event_type()) {
            continue;
        }

        let envelope = match parse::parse_event(&cfg.mdt, &view) {
            Ok(Some(env)) => env,
            Ok(None) => continue,
            Err(e) => {
                warn!(mdt = %cfg.mdt, err = %e, "failed to parse record; skipping");
                continue;
            }
        };

        drop(buf);

        if batcher.push(envelope)
            && let Some(batch) = batcher.flush()
            && event_tx.blocking_send(batch).is_err()
        {
            return Err(ChangelogError::EventChannelClosed);
        }
    }

    if let Some(batch) = batcher.flush() {
        let count = batch.events.len();
        if event_tx.blocking_send(batch).is_err() {
            warn!(mdt = %cfg.mdt, dropped_events = count, "consumer gone; partial batch dropped on shutdown");
        }
    }

    process_acks(
        &api,
        &cfg.mdt,
        &cfg.reader_id,
        &mut ack_rx,
        &mut ack_state,
        &cursor_store,
    );
    api.close_changelog(handle)?;
    info!(mdt = %cfg.mdt, last_cleared = ack_state.last_cleared, "changelog listener stopped");
    Ok(())
}

/// Mutable state tracked across ack processing rounds.
struct AckState {
    last_cleared: i64,
    records_since_commit: u64,
    last_commit_time: std::time::Instant,
}

/// Non-blocking drain of pending acks → drive clear_changelog + cursor commits.
#[allow(clippy::collapsible_if)] // nested ifs are clearer here for error handling
fn process_acks(
    api: &LustreApi, mdt: &str, reader_id: &str, ack_rx: &mut mpsc::Receiver<EventAck>, state: &mut AckState,
    cursor_store: &Arc<dyn CursorStore>,
) {
    while let Ok(ack) = ack_rx.try_recv() {
        let idx = ack.committed_index as i64;
        if idx > state.last_cleared {
            match api.clear_changelog(mdt, reader_id, idx) {
                Ok(()) => {
                    state.last_cleared = idx;
                    // M4 fix: only increment on successful clear.
                    state.records_since_commit += 1;
                }
                Err(e) => warn!(mdt, idx, err = %e, "clear_changelog failed"),
            }
        }
    }

    // Periodic cursor commit: every 1000 records or 5s.
    let should_commit =
        state.records_since_commit >= 1000 || state.last_commit_time.elapsed() >= Duration::from_secs(5);
    // H2 fix: use >= 0 so position 0 is committable.
    if should_commit && state.last_cleared >= 0 {
        let idx = state.last_cleared as u64;
        let store = Arc::clone(cursor_store);
        let mdt_owned = mdt.to_owned();
        // H1 fix: use Handle::spawn instead of block_on to avoid panic on
        // current_thread runtime (used by #[tokio::test]). Fire-and-forget
        // — cursor commit is best-effort; next startup resumes from the last
        // successfully committed position.
        if let Ok(rt) = tokio::runtime::Handle::try_current() {
            drop(rt.spawn(async move {
                if let Err(e) = store.commit(&mdt_owned, idx).await {
                    tracing::warn!(mdt = mdt_owned, idx, err = %e, "cursor commit failed");
                }
            }));
        }
        state.records_since_commit = 0;
        state.last_commit_time = std::time::Instant::now();
    }
}
